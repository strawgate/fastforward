//! End-to-end benchmark harness. Runs the full pipeline locally:
//!   read file → CRI parse → JSON field inject → encode → compress → /dev/null
//!
//! No networking, no Docker, no K8s. Pure processing throughput measurement
//! with per-stage timing breakdown.

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use std::time::Instant;

use crate::chunk::{ChunkAccumulator, ChunkConfig, OwnedChunk};
use crate::compress::ChunkCompressor;
use crate::cri::{self, CriReassembler};
use crate::otlp;

/// What output encoding to benchmark.
#[derive(Clone, Debug)]
pub enum E2eMode {
    /// Just read + CRI parse + count lines. No encoding.
    CriOnly,
    /// CRI parse → raw JSON lines (what we send to /insert/jsonline).
    JsonLines,
    /// CRI parse → OTLP protobuf (with JSON field extraction).
    OtlpProto,
    /// CRI parse → OTLP protobuf (raw body, no JSON parsing).
    OtlpRaw,
    /// CRI parse → JSON lines → zstd compress.
    JsonLinesCompressed,
    /// CRI parse → OTLP protobuf (JSON extracted) → zstd compress.
    OtlpCompressed,
    /// CRI parse → OTLP protobuf (raw body) → zstd compress.
    OtlpRawCompressed,
}

/// Per-stage timing breakdown.
#[derive(Default)]
pub struct E2eTimings {
    pub read_ns: u64,
    pub cri_parse_ns: u64,
    pub json_inject_ns: u64,
    pub otlp_encode_ns: u64,
    pub compress_ns: u64,
    pub total_lines: u64,
    pub total_bytes_in: u64,
    pub total_bytes_out: u64,
    pub elapsed_ns: u64,
}

impl E2eTimings {
    pub fn print_report(&self) {
        let elapsed_s = self.elapsed_ns as f64 / 1e9;
        let lps = self.total_lines as f64 / elapsed_s;
        let mbps_in = self.total_bytes_in as f64 / (1024.0 * 1024.0) / elapsed_s;
        let total_work = self.read_ns + self.cri_parse_ns + self.json_inject_ns
            + self.otlp_encode_ns + self.compress_ns;
        let pct = |ns: u64| {
            if total_work > 0 { ns as f64 / total_work as f64 * 100.0 } else { 0.0 }
        };

        eprintln!("=== End-to-end benchmark results ===");
        eprintln!("  Lines:       {:>12}", format_num(self.total_lines));
        eprintln!("  Input:       {:>12}", format_bytes(self.total_bytes_in));
        eprintln!("  Output:      {:>12}", format_bytes(self.total_bytes_out));
        if self.total_bytes_out > 0 && self.total_bytes_out != self.total_bytes_in {
            eprintln!("  Ratio:       {:>11.1}x",
                self.total_bytes_in as f64 / self.total_bytes_out as f64);
        }
        eprintln!("  Elapsed:     {:>10.3}s", elapsed_s);
        eprintln!("  Throughput:  {:>10.0} lines/sec", lps);
        eprintln!("  Input rate:  {:>10.1} MB/sec", mbps_in);
        eprintln!();
        eprintln!("  Stage breakdown:");
        eprintln!("    read:        {:>5.1}%  ({:.1}ms)", pct(self.read_ns), self.read_ns as f64 / 1e6);
        eprintln!("    cri_parse:   {:>5.1}%  ({:.1}ms)", pct(self.cri_parse_ns), self.cri_parse_ns as f64 / 1e6);
        eprintln!("    json_inject: {:>5.1}%  ({:.1}ms)", pct(self.json_inject_ns), self.json_inject_ns as f64 / 1e6);
        eprintln!("    otlp_encode: {:>5.1}%  ({:.1}ms)", pct(self.otlp_encode_ns), self.otlp_encode_ns as f64 / 1e6);
        eprintln!("    compress:    {:>5.1}%  ({:.1}ms)", pct(self.compress_ns), self.compress_ns as f64 / 1e6);
        eprintln!();

        let target = 1_000_000.0;
        if lps >= target {
            eprintln!("  >>> TARGET HIT: {:.1}x above 1M lines/sec", lps / target);
        } else {
            eprintln!("  Below target: {:.1}% of 1M lines/sec", lps / target * 100.0);
        }
    }
}

fn format_num(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

fn format_bytes(b: u64) -> String {
    if b >= 1_073_741_824 {
        format!("{:.2} GB", b as f64 / 1_073_741_824.0)
    } else if b >= 1_048_576 {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    } else if b >= 1024 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

/// Run the end-to-end benchmark.
pub fn run_e2e<P: AsRef<Path>>(path: P, mode: E2eMode) -> io::Result<E2eTimings> {
    let file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let mut reader = BufReader::with_capacity(1024 * 1024, file);

    let mut timings = E2eTimings::default();
    let mut reassembler = CriReassembler::new(2 * 1024 * 1024);
    let mut compressor = ChunkCompressor::new(1);
    let mut otlp_encoder = otlp::BatchEncoder::new();

    // Buffers reused across iterations.
    let mut read_buf = vec![0u8; 1024 * 1024];
    let mut json_batch = Vec::with_capacity(4 * 1024 * 1024);

    let observed_time_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    let start = Instant::now();

    loop {
        // Stage 1: Read
        let t = Instant::now();
        let n = reader.read(&mut read_buf)?;
        timings.read_ns += t.elapsed().as_nanos() as u64;

        if n == 0 {
            break;
        }
        timings.total_bytes_in += n as u64;

        // Stage 2+3: depends on output mode.
        let t = Instant::now();
        let line_count;

        match mode {
            E2eMode::CriOnly => {
                // Just parse, count lines, discard.
                line_count = cri::process_cri_chunk(&read_buf[..n], &mut reassembler, |_| {});
                timings.cri_parse_ns += t.elapsed().as_nanos() as u64;
                timings.total_lines += line_count as u64;
                continue;
            }
            E2eMode::JsonLines | E2eMode::JsonLinesCompressed => {
                // CRI parse + JSON inject directly into json_batch.
                json_batch.clear();
                let prefix = b"\"kubernetes.pod_name\":\"test-pod\",";
                line_count = cri::process_cri_to_buf(
                    &read_buf[..n], &mut reassembler, Some(prefix), &mut json_batch,
                );
                timings.cri_parse_ns += t.elapsed().as_nanos() as u64;
                timings.total_lines += line_count as u64;

                if matches!(mode, E2eMode::JsonLines) {
                    timings.total_bytes_out += json_batch.len() as u64;
                } else {
                    let t = Instant::now();
                    let compressed = compressor.compress(&json_batch)?;
                    timings.compress_ns += t.elapsed().as_nanos() as u64;
                    timings.total_bytes_out += compressed.compressed_size as u64;
                }
                continue;
            }
            E2eMode::OtlpProto | E2eMode::OtlpCompressed
            | E2eMode::OtlpRaw | E2eMode::OtlpRawCompressed => {
                // CRI parse → raw messages → OTLP encode.
                json_batch.clear();
                line_count = cri::process_cri_to_buf(
                    &read_buf[..n], &mut reassembler, None, &mut json_batch,
                );
                timings.cri_parse_ns += t.elapsed().as_nanos() as u64;
                timings.total_lines += line_count as u64;

                let t = Instant::now();
                let raw_mode = matches!(mode, E2eMode::OtlpRaw | E2eMode::OtlpRawCompressed);
                let otlp_bytes = if raw_mode {
                    otlp_encoder.encode_from_buf_raw(&json_batch, observed_time_ns)
                } else {
                    otlp_encoder.encode_from_buf(&json_batch, observed_time_ns)
                };
                timings.otlp_encode_ns += t.elapsed().as_nanos() as u64;

                if matches!(mode, E2eMode::OtlpCompressed | E2eMode::OtlpRawCompressed) {
                    let t = Instant::now();
                    let compressed = compressor.compress(&otlp_bytes)?;
                    timings.compress_ns += t.elapsed().as_nanos() as u64;
                    timings.total_bytes_out += compressed.compressed_size as u64;
                } else {
                    timings.total_bytes_out += otlp_bytes.len() as u64;
                }
                continue;
            }
        }
    }

    timings.elapsed_ns = start.elapsed().as_nanos() as u64;
    Ok(timings)
}

/// Generate a CRI-wrapped test file from a JSON log file.
pub fn generate_cri_file<P: AsRef<Path>>(json_path: P, cri_path: P) -> io::Result<()> {
    let input = std::fs::read(json_path)?;
    let mut output = std::io::BufWriter::new(File::create(cri_path)?);

    let mut line_num = 0u64;
    let mut start = 0;
    for pos in memchr::memchr_iter(b'\n', &input) {
        let line = &input[start..pos];
        start = pos + 1;
        if line.is_empty() {
            continue;
        }
        line_num += 1;
        // Write CRI format: "timestamp stdout F message\n"
        write!(
            output,
            "2026-03-28T19:{:02}:{:02}.{:06}Z stdout F ",
            (line_num / 3600) % 60,
            (line_num / 60) % 60,
            line_num % 1_000_000,
        )?;
        output.write_all(line)?;
        output.write_all(b"\n")?;
    }

    output.flush()?;
    Ok(())
}
