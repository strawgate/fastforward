//! Daemon mode: tail container log files, parse CRI, send to a remote endpoint.
//!
//! Architecture: two threads.
//! - Reader thread: polls files, parses CRI, fills batches, pushes to queue
//! - Sender thread: drains queue, POSTs to endpoint
//! Reader never blocks on network. Sender never blocks on disk.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cri::{self, CriReassembler};
use crate::read_tuner::ReadTuner;
use crate::sender::{HttpSender, SenderConfig};

/// Daemon configuration.
pub struct DaemonConfig {
    pub glob_pattern: String,
    pub endpoint: String,
    pub collector_name: String,
    pub compress: bool,
    pub batch_max_bytes: usize,
    pub batch_timeout: Duration,
    pub discovery_interval: Duration,
    pub poll_interval: Duration,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            glob_pattern: "/var/log/containers/*log-generator*.log".into(),
            endpoint: "http://log-verifier.monitoring.svc.cluster.local.:8080/insert/jsonline".into(),
            collector_name: "logfwd".into(),
            compress: false,
            batch_max_bytes: 4 * 1024 * 1024, // 4MB batches — fewer HTTP round trips
            batch_timeout: Duration::from_millis(100),
            discovery_interval: Duration::from_secs(5),
            poll_interval: Duration::from_millis(10),
        }
    }
}

struct TailedFile {
    file: File,
    offset: u64,
    inode: u64,
    reassembler: CriReassembler,
    pod_name: String,
    /// Cached resolved real path (avoid canonicalize per-poll).
    real_path: PathBuf,
}

fn extract_pod_name(path: &Path) -> String {
    let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
    filename.split('_').next().unwrap_or("unknown").to_string()
}

/// Run the daemon with reader + sender threads.
pub fn run_daemon(config: DaemonConfig) -> io::Result<()> {
    eprintln!("logfwd daemon starting");
    eprintln!("  glob: {}", config.glob_pattern);
    eprintln!("  endpoint: {}", config.endpoint);
    eprintln!("  collector: {}", config.collector_name);
    eprintln!("  batch_max: {} KB", config.batch_max_bytes / 1024);

    // Bounded channel between reader and sender. 32 slots = ~16MB buffered.
    let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(32);

    // Sender thread: drains batches and POSTs them.
    let sender_config = SenderConfig {
        endpoint: config.endpoint.clone(),
        extra_fields: format!("collector={}", config.collector_name),
        stream_fields: "collector".into(),
        compress: config.compress,
        timeout: Duration::from_secs(30),
    };

    // Spawn multiple sender threads for concurrent HTTP POSTs.
    let num_senders = 4;
    let mut sender_handles = Vec::new();
    for sender_id in 0..num_senders {
        let rx = rx.clone();
        let sender_config = sender_config.clone();
        sender_handles.push(std::thread::spawn(move || {
            let sender = HttpSender::new(sender_config);
            let mut total_sent = 0u64;
            let mut total_batches = 0u64;
            let mut total_errors = 0u64;

            while let Ok(batch) = rx.recv() {
                match sender.send(&batch) {
                    Ok(_) => {
                        total_sent += batch.len() as u64;
                        total_batches += 1;
                    }
                    Err(e) => {
                        total_errors += 1;
                        if total_errors <= 10 || total_errors % 100 == 0 {
                            eprintln!("  sender[{sender_id}] error #{total_errors}: {e}");
                        }
                    }
                }
            }
            eprintln!("  sender[{sender_id}] done: {total_batches} batches, {:.1} MB, {total_errors} errors",
                total_sent as f64 / (1024.0 * 1024.0));
        }));
    }
    drop(rx); // drop our copy so senders see disconnect when reader drops tx

    // Reader thread (runs on main thread).
    let result = run_reader_loop(&config, tx);

    drop(result);
    for h in sender_handles {
        let _ = h.join();
    }
    Ok(())
}

fn run_reader_loop(
    config: &DaemonConfig,
    tx: crossbeam_channel::Sender<Vec<u8>>,
) -> io::Result<()> {
    let mut files: HashMap<PathBuf, TailedFile> = HashMap::new();
    let mut read_tuner = ReadTuner::new();
    let mut batch = Vec::with_capacity(config.batch_max_bytes + 4096);
    let mut batch_lines = 0usize;
    let mut last_flush = Instant::now();
    let mut last_discovery = Instant::now() - config.discovery_interval;

    let mut total_lines = 0u64;
    let mut total_bytes_in = 0u64;
    let mut last_report = Instant::now();
    let mut interval_lines = 0u64;
    let mut interval_bytes_in = 0u64;
    let mut send_stalls = 0u64;

    // Per-stage timing (nanoseconds, reset each reporting interval).
    let mut ns_read = 0u64;
    let mut ns_cri = 0u64;
    let mut ns_send = 0u64;
    let mut ns_meta = 0u64; // metadata/stat checks

    loop {
        // File discovery.
        if last_discovery.elapsed() >= config.discovery_interval {
            let discovered = discover_files(&config.glob_pattern);
            for path in &discovered {
                if !files.contains_key(path) {
                    let real_path = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                    let pod_name = extract_pod_name(path);
                    match open_tail_file(&real_path, &pod_name) {
                        Ok(tf) => {
                            eprintln!("  [new] {} (pod: {})", path.display(), pod_name);
                            files.insert(path.clone(), tf);
                        }
                        Err(e) => eprintln!("  [err] {}: {e}", path.display()),
                    }
                }
            }
            files.retain(|path, _| path.exists());
            last_discovery = Instant::now();
        }

        // Read from all files.
        let mut had_data = false;
        let file_paths: Vec<PathBuf> = files.keys().cloned().collect();
        for path in file_paths {
            let tf = files.get_mut(&path).unwrap();

            // Drain all available data — skip metadata checks while data flows.
            // Only check rotation/truncation when read() returns 0 (no new data).
            loop {
                let read_start = Instant::now();
                let n = match tf.file.read(read_tuner.buf_mut()) {
                    Ok(0) => {
                        let elapsed = read_start.elapsed().as_nanos() as u64;
                        ns_read += elapsed;
                        // No data — now check for rotation/truncation.
                        let meta_start = Instant::now();
                        if let Ok(meta) = fs::metadata(&tf.real_path) {
                            if meta.ino() != tf.inode {
                                eprintln!("  [rotated] {}", path.display());
                                if let Ok(new_tf) = open_tail_file(&tf.real_path, &tf.pod_name) {
                                    *tf = new_tf;
                                }
                            } else if meta.len() < tf.offset {
                                tf.offset = 0;
                                let _ = tf.file.seek(SeekFrom::Start(0));
                                tf.reassembler.reset();
                            }
                        }
                        ns_meta += meta_start.elapsed().as_nanos() as u64;
                        break;
                    }
                    Ok(n) => n,
                    Err(_) => break,
                };
                let read_elapsed = read_start.elapsed().as_nanos() as u64;
                ns_read += read_elapsed;
                read_tuner.record(n, read_elapsed);
                had_data = true;
                tf.offset += n as u64;
                total_bytes_in += n as u64;
                interval_bytes_in += n as u64;

                let cri_start = Instant::now();
                let pod_name = &tf.pod_name;
                cri::process_cri_chunk(&read_tuner.buf()[..n], &mut tf.reassembler, |msg| {
                    if msg.first() == Some(&b'{') {
                        batch.push(b'{');
                        batch.extend_from_slice(b"\"kubernetes.pod_name\":\"");
                        batch.extend_from_slice(pod_name.as_bytes());
                        batch.extend_from_slice(b"\",");
                        batch.extend_from_slice(&msg[1..]);
                    } else {
                        batch.extend_from_slice(msg);
                    }
                    batch.push(b'\n');
                    batch_lines += 1;
                });
                ns_cri += cri_start.elapsed().as_nanos() as u64;

                // Flush mid-file if batch is full — don't accumulate unbounded.
                if batch.len() >= config.batch_max_bytes {
                    let to_send = std::mem::replace(&mut batch, Vec::with_capacity(config.batch_max_bytes + 4096));
                    total_lines += batch_lines as u64;
                    interval_lines += batch_lines as u64;
                    batch_lines = 0;
                    last_flush = Instant::now();

                    let send_start = Instant::now();
                    match tx.try_send(to_send) {
                        Ok(()) => {}
                        Err(crossbeam_channel::TrySendError::Full(data)) => {
                            send_stalls += 1;
                            let _ = tx.send(data);
                        }
                        Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                            return Ok(());
                        }
                    }
                    ns_send += send_start.elapsed().as_nanos() as u64;
                }
            }
        }

        // Flush on timeout.
        if batch_lines > 0 && last_flush.elapsed() >= config.batch_timeout {
            let to_send = std::mem::replace(&mut batch, Vec::with_capacity(config.batch_max_bytes + 4096));
            total_lines += batch_lines as u64;
            interval_lines += batch_lines as u64;
            batch_lines = 0;
            last_flush = Instant::now();
            let _ = tx.try_send(to_send); // best effort on timeout flush
        }

        // Stats.
        if last_report.elapsed() >= Duration::from_secs(5) {
            let elapsed = last_report.elapsed().as_secs_f64();
            let lps = interval_lines as f64 / elapsed;
            let mbps = interval_bytes_in as f64 / (1024.0 * 1024.0) / elapsed;
            let total_ns = ns_read + ns_cri + ns_send + ns_meta;
            let pct = |ns: u64| if total_ns > 0 { ns as f64 / total_ns as f64 * 100.0 } else { 0.0 };
            eprintln!(
                "  {:.0} lps | {:.1} MB/s | {} total | read:{:.0}% cri:{:.0}% send:{:.0}% meta:{:.0}% | {} stalls",
                lps, mbps, total_lines, pct(ns_read), pct(ns_cri), pct(ns_send), pct(ns_meta), send_stalls,
            );
            interval_lines = 0;
            interval_bytes_in = 0;
            ns_read = 0;
            ns_cri = 0;
            ns_send = 0;
            ns_meta = 0;
            last_report = Instant::now();
        }

        if !had_data {
            std::thread::sleep(config.poll_interval);
        }
    }
}

fn open_tail_file(real_path: &Path, pod_name: &str) -> io::Result<TailedFile> {
    let meta = fs::metadata(real_path)?;
    let file = File::open(real_path)?;
    Ok(TailedFile {
        file, offset: 0, inode: meta.ino(),
        reassembler: CriReassembler::new(2 * 1024 * 1024),
        pod_name: pod_name.to_string(),
        real_path: real_path.to_path_buf(),
    })
}

fn discover_files(pattern: &str) -> Vec<PathBuf> {
    let path = Path::new(pattern);
    let dir = path.parent().unwrap_or(Path::new("."));
    let file_pattern = path.file_name().and_then(|f| f.to_str()).unwrap_or("*");
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut results = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if simple_glob_match(file_pattern, &name.to_string_lossy()) {
            results.push(entry.path());
        }
    }
    results.sort();
    results
}

fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 { return pattern == text; }
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() { continue; }
        match text[pos..].find(part) {
            Some(found) => { if i == 0 && found != 0 { return false; } pos += found + part.len(); }
            None => return false,
        }
    }
    if !pattern.ends_with('*') { return pos == text.len(); }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_glob() {
        assert!(simple_glob_match("*.log", "test.log"));
        assert!(simple_glob_match("*log-generator*.log", "something-log-generator-abc.log"));
        assert!(!simple_glob_match("*.log", "test.txt"));
    }

    #[test]
    fn test_extract_pod_name() {
        let path = Path::new("/var/log/containers/my-pod-abc123_default_container-xyz.log");
        assert_eq!(extract_pod_name(path), "my-pod-abc123");
    }
}
