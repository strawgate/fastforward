//! UDP syslog receiver benchmark.
//!
//! Measures throughput of recvmmsg-based batch UDP reception with
//! syslog priority parsing and severity filtering. This is the
//! non-eBPF baseline for comparison with the XDP syslog filter.
//!
//! Usage:
//!   udp-bench receive [--port 5514] [--severity 4] [--batch 64]
//!   udp-bench generate [--port 5514] [--count 1000000] [--pps 0]

use std::env;
use std::io;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

// ---- Syslog priority parser ----

/// Parsed syslog priority metadata.
#[derive(Debug, Clone, Copy)]
struct SyslogMeta {
    facility: u8,
    severity: u8,
    pri_len: u8,  // bytes consumed including < and >
}

/// Parse syslog <priority> from the start of a buffer.
/// Returns None if the buffer doesn't start with a valid <NNN> prefix.
///
/// This is the scalar baseline. On a modern CPU this takes ~3-8ns per call
/// (branch-predicted, no memory allocation, no SIMD needed for 3-5 bytes).
#[inline(always)]
fn parse_syslog_pri(buf: &[u8]) -> Option<SyslogMeta> {
    if buf.len() < 3 || buf[0] != b'<' {
        return None;
    }

    let mut pri: u16 = 0;
    let mut i = 1;

    // Parse up to 3 digits
    while i < buf.len() && i <= 4 {
        let b = buf[i];
        if b == b'>' {
            let facility = (pri >> 3) as u8;
            let severity = (pri & 0x7) as u8;
            return Some(SyslogMeta {
                facility,
                severity,
                pri_len: (i + 1) as u8,
            });
        }
        if !b.is_ascii_digit() {
            return None;
        }
        pri = pri * 10 + (b - b'0') as u16;
        i += 1;
    }

    None
}

// ---- recvmmsg wrapper ----

/// Receive a batch of UDP packets using recvmmsg(2).
/// Returns the number of messages received.
///
/// recvmmsg is the key syscall for high-throughput UDP — it pulls up to
/// `batch_size` packets in a single syscall, amortizing the syscall overhead.
unsafe fn recvmmsg_batch(
    fd: i32,
    bufs: &mut [Vec<u8>],
    iovecs: &mut [libc::iovec],
    msgvec: &mut [libc::mmsghdr],
) -> io::Result<usize> {
    let batch = bufs.len();

    // Set up scatter-gather for each message
    for i in 0..batch {
        iovecs[i] = libc::iovec {
            iov_base: bufs[i].as_mut_ptr() as *mut _,
            iov_len: bufs[i].len(),
        };
        msgvec[i] = unsafe { std::mem::zeroed() };
        msgvec[i].msg_hdr.msg_iov = &mut iovecs[i];
        msgvec[i].msg_hdr.msg_iovlen = 1;
    }

    // Non-blocking recvmmsg with no timeout
    let ret = libc::recvmmsg(
        fd,
        msgvec.as_mut_ptr(),
        batch as u32,
        libc::MSG_DONTWAIT,
        std::ptr::null_mut(),
    );

    if ret < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            return Ok(0);
        }
        return Err(err);
    }

    Ok(ret as usize)
}

// ---- Receiver ----

fn run_receiver(port: u16, severity_threshold: u8, batch_size: usize) -> io::Result<()> {
    let sock = UdpSocket::bind(format!("0.0.0.0:{}", port))?;
    sock.set_nonblocking(true)?;
    let fd = {
        use std::os::unix::io::AsRawFd;
        sock.as_raw_fd()
    };

    eprintln!("Listening on UDP port {}, severity <= {}, batch size {}",
              port, severity_threshold, batch_size);

    // Pre-allocate buffers
    let mut bufs: Vec<Vec<u8>> = (0..batch_size).map(|_| vec![0u8; 2048]).collect();
    let mut iovecs: Vec<libc::iovec> = vec![unsafe { std::mem::zeroed() }; batch_size];
    let mut msgvec: Vec<libc::mmsghdr> = vec![unsafe { std::mem::zeroed() }; batch_size];

    let mut total_packets: u64 = 0;
    let mut total_matched: u64 = 0;
    let mut total_dropped: u64 = 0;
    let mut total_parse_fail: u64 = 0;
    let mut total_bytes: u64 = 0;

    let start = Instant::now();
    let mut last_report = start;
    let mut interval_packets: u64 = 0;
    let mut interval_matched: u64 = 0;

    loop {
        let n = unsafe { recvmmsg_batch(fd, &mut bufs, &mut iovecs, &mut msgvec)? };

        if n == 0 {
            // No data — check if we should report stats
            let now = Instant::now();
            if now.duration_since(last_report) >= Duration::from_secs(1) {
                if total_packets > 0 {
                    report_stats(start, &mut last_report, now,
                                 interval_packets, interval_matched,
                                 total_packets, total_matched, total_dropped,
                                 total_parse_fail, total_bytes);
                    interval_packets = 0;
                    interval_matched = 0;
                }
            }
            // Brief sleep to avoid busy-spinning when idle
            std::thread::sleep(Duration::from_micros(100));
            continue;
        }

        // Process batch
        for i in 0..n {
            let len = msgvec[i].msg_len as usize;
            let buf = &bufs[i][..len];
            total_bytes += len as u64;
            total_packets += 1;
            interval_packets += 1;

            match parse_syslog_pri(buf) {
                Some(meta) => {
                    if meta.severity <= severity_threshold {
                        total_matched += 1;
                        interval_matched += 1;
                        // In a real pipeline, matched packets would be
                        // forwarded to the Arrow scanner here.
                    } else {
                        total_dropped += 1;
                    }
                }
                None => {
                    total_parse_fail += 1;
                }
            }
        }

        // Periodic reporting
        let now = Instant::now();
        if now.duration_since(last_report) >= Duration::from_secs(1) {
            report_stats(start, &mut last_report, now,
                         interval_packets, interval_matched,
                         total_packets, total_matched, total_dropped,
                         total_parse_fail, total_bytes);
            interval_packets = 0;
            interval_matched = 0;
        }
    }
}

fn report_stats(
    start: Instant, last_report: &mut Instant, now: Instant,
    interval_pkts: u64, interval_matched: u64,
    total_pkts: u64, total_matched: u64, total_dropped: u64,
    total_fail: u64, total_bytes: u64,
) {
    let interval = now.duration_since(*last_report).as_secs_f64();
    let total_secs = now.duration_since(start).as_secs_f64();
    let pps = interval_pkts as f64 / interval;
    let matched_pps = interval_matched as f64 / interval;
    let avg_pps = total_pkts as f64 / total_secs;
    let mbps = (total_bytes as f64 / total_secs) / (1024.0 * 1024.0);

    eprintln!(
        "[{:.1}s] {:.0} pps ({:.0} matched) | total: {} pkts, {} matched, {} dropped, {} failed | {:.1} MB/s avg {:.0} pps",
        total_secs, pps, matched_pps,
        total_pkts, total_matched, total_dropped, total_fail, mbps, avg_pps,
    );
    *last_report = now;
}

// ---- Generator ----

fn run_generator(port: u16, count: u64, target_pps: u64) -> io::Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    let dest = format!("127.0.0.1:{}", port);

    // Pre-generate messages for all 8 severity levels
    let sev_names = ["EMERG", "ALERT", "CRIT", "ERR", "WARN", "NOTICE", "INFO", "DEBUG"];
    let messages: Vec<Vec<u8>> = (0..8).map(|sev| {
        let pri = (16 << 3) | sev; // facility=local0
        format!(
            "<{}>Jan 15 10:30:45 benchhost myapp[1234]: severity={} seq=XXXXXXXX this is a test syslog message with some realistic payload padding",
            pri, sev_names[sev as usize]
        ).into_bytes()
    }).collect();

    eprintln!("Generating {} syslog packets to {} (target: {} pps)",
              count, dest,
              if target_pps == 0 { "max".to_string() } else { format!("{}", target_pps) });

    let start = Instant::now();
    let mut sent: u64 = 0;
    let mut last_report = start;
    let mut interval_sent: u64 = 0;

    // Rate limiting: if target_pps > 0, pace sends
    let send_interval = if target_pps > 0 {
        Some(Duration::from_nanos(1_000_000_000 / target_pps))
    } else {
        None
    };
    let mut next_send = Instant::now();

    while sent < count {
        if let Some(interval) = send_interval {
            let now = Instant::now();
            if now < next_send {
                std::thread::sleep(next_send - now);
            }
            next_send += interval;
        }

        let sev = (sent % 8) as usize;
        sock.send_to(&messages[sev], &dest)?;
        sent += 1;
        interval_sent += 1;

        let now = Instant::now();
        if now.duration_since(last_report) >= Duration::from_secs(1) {
            let elapsed = now.duration_since(start).as_secs_f64();
            let pps = interval_sent as f64 / now.duration_since(last_report).as_secs_f64();
            eprintln!("[{:.1}s] sent {} ({:.0} pps)", elapsed, sent, pps);
            last_report = now;
            interval_sent = 0;
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let avg_pps = sent as f64 / elapsed;
    eprintln!("Done: {} packets in {:.2}s ({:.0} pps avg)", sent, elapsed, avg_pps);
    Ok(())
}

// ---- CLI ----

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage:");
        eprintln!("  udp-bench receive [--port 5514] [--severity 4] [--batch 64]");
        eprintln!("  udp-bench generate [--port 5514] [--count 1000000] [--pps 0]");
        eprintln!();
        eprintln!("The receiver listens for UDP syslog, parses priority, filters by");
        eprintln!("severity, and reports throughput. Compare with XDP filter path.");
        std::process::exit(1);
    }

    let mode = &args[1];
    let get_arg = |name: &str, default: &str| -> String {
        args.windows(2)
            .find(|w| w[0] == format!("--{}", name))
            .map(|w| w[1].clone())
            .unwrap_or_else(|| default.to_string())
    };

    match mode.as_str() {
        "receive" => {
            let port: u16 = get_arg("port", "5514").parse().unwrap();
            let sev: u8 = get_arg("severity", "4").parse().unwrap();
            let batch: usize = get_arg("batch", "64").parse().unwrap();
            run_receiver(port, sev, batch)
        }
        "generate" => {
            let port: u16 = get_arg("port", "5514").parse().unwrap();
            let count: u64 = get_arg("count", "1000000").parse().unwrap();
            let pps: u64 = get_arg("pps", "0").parse().unwrap();
            run_generator(port, count, pps)
        }
        _ => {
            eprintln!("Unknown mode: {}. Use 'receive' or 'generate'.", mode);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_priority_basic() {
        let m = parse_syslog_pri(b"<134>test").unwrap();
        assert_eq!(m.facility, 16); // 134 >> 3 = 16
        assert_eq!(m.severity, 6);  // 134 & 7 = 6
        assert_eq!(m.pri_len, 5);   // "<134>"
    }

    #[test]
    fn parse_priority_single_digit() {
        let m = parse_syslog_pri(b"<0>test").unwrap();
        assert_eq!(m.facility, 0);
        assert_eq!(m.severity, 0);
        assert_eq!(m.pri_len, 3);
    }

    #[test]
    fn parse_priority_two_digit() {
        let m = parse_syslog_pri(b"<13>test").unwrap();
        assert_eq!(m.facility, 1);
        assert_eq!(m.severity, 5);
        assert_eq!(m.pri_len, 4);
    }

    #[test]
    fn parse_priority_max() {
        let m = parse_syslog_pri(b"<191>test").unwrap();
        assert_eq!(m.facility, 23);
        assert_eq!(m.severity, 7);
    }

    #[test]
    fn parse_priority_invalid() {
        assert!(parse_syslog_pri(b"no angle bracket").is_none());
        assert!(parse_syslog_pri(b"<abc>test").is_none());
        assert!(parse_syslog_pri(b"<1234>test").is_none()); // 4+ digits
        assert!(parse_syslog_pri(b"<13").is_none());         // no closing >
        assert!(parse_syslog_pri(b"").is_none());
    }

    #[test]
    fn parse_priority_throughput() {
        let buf = b"<134>Jan 15 10:30:45 host app[1234]: test message";
        let start = std::time::Instant::now();
        let n = 10_000_000;
        for _ in 0..n {
            let m = parse_syslog_pri(std::hint::black_box(buf));
            std::hint::black_box(m);
        }
        let elapsed = start.elapsed();
        let ns_per = elapsed.as_nanos() as f64 / n as f64;
        eprintln!("parse_syslog_pri: {:.1}ns/call ({:.1}M/sec)",
                  ns_per, 1000.0 / ns_per);
    }
}
