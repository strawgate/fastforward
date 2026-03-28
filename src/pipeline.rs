//! The core pipeline: read → line split → encode → compress.
//!
//! Supports multiple output modes:
//! - Raw chunks (compressed newline-delimited bytes)
//! - OTLP protobuf (JSON lines → protobuf LogRecords → compressed)
//! - Passthrough (no encoding, for benchmarking raw read speed)

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::time::Instant;

use crate::chunk::{ChunkAccumulator, ChunkConfig, ChunkStats, OwnedChunk};
use crate::compress::ChunkCompressor;
use crate::otlp;
use crate::tuner::ChunkTuner;

/// What to do with each chunk of log data.
#[derive(Clone, Debug, PartialEq)]
pub enum OutputMode {
    /// Compress raw bytes. Zero per-line overhead.
    RawChunk,
    /// JSON lines → OTLP protobuf LogRecords → compress.
    Otlp,
    /// No encoding or compression — just read and count lines.
    Passthrough,
}

/// Pipeline statistics for a complete run.
#[derive(Clone, Debug, Default)]
pub struct PipelineStats {
    pub total_raw_bytes: u64,
    pub total_output_bytes: u64,
    pub total_chunks: u64,
    pub total_lines: u64,
    pub elapsed_ns: u64,
    pub final_chunk_size: usize,
    pub tuner_scores: Vec<(usize, f64, f64, f64)>,
}

impl PipelineStats {
    pub fn throughput_mbps(&self) -> f64 {
        if self.elapsed_ns == 0 {
            return 0.0;
        }
        (self.total_raw_bytes as f64 / (1024.0 * 1024.0))
            / (self.elapsed_ns as f64 / 1_000_000_000.0)
    }

    pub fn lines_per_sec(&self) -> f64 {
        if self.elapsed_ns == 0 {
            return 0.0;
        }
        self.total_lines as f64 / (self.elapsed_ns as f64 / 1_000_000_000.0)
    }

    pub fn avg_ratio(&self) -> f64 {
        if self.total_output_bytes == 0 {
            return 0.0;
        }
        self.total_raw_bytes as f64 / self.total_output_bytes as f64
    }
}

/// Configuration for the pipeline.
pub struct PipelineConfig {
    pub chunk: ChunkConfig,
    pub compression_level: i32,
    pub adaptive: bool,
    pub mode: OutputMode,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            chunk: ChunkConfig::default(),
            compression_level: 1,
            adaptive: true,
            mode: OutputMode::RawChunk,
        }
    }
}

/// Run the pipeline on a file.
pub fn run_file<P: AsRef<Path>>(path: P, config: PipelineConfig) -> io::Result<PipelineStats> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(config.chunk.target_size * 2, file);
    run_reader(&mut reader, config)
}

/// Run the pipeline on any Read source.
pub fn run_reader<R: Read>(reader: &mut R, config: PipelineConfig) -> io::Result<PipelineStats> {
    let mut accumulator = ChunkAccumulator::new(config.chunk.clone());
    let mut compressor = ChunkCompressor::new(config.compression_level);
    let mut tuner = if config.adaptive {
        Some(ChunkTuner::new(
            config.chunk.target_size,
            config.chunk.min_size,
            config.chunk.max_size,
        ))
    } else {
        None
    };

    let mut stats = PipelineStats::default();
    let start = Instant::now();
    let observed_time_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    loop {
        // Fill until we have enough data or hit EOF.
        let read_start = Instant::now();
        let mut total_read = 0usize;
        loop {
            let n = accumulator.fill_from(reader)?;
            total_read += n;
            if n == 0 || accumulator.buffered() >= accumulator.target_size() {
                break;
            }
        }
        let read_ns = read_start.elapsed().as_nanos() as u64;

        if total_read == 0 && accumulator.is_empty() {
            break;
        }

        // Take the chunk — ownership transfers to us via swap. No borrow conflicts.
        let flush = total_read == 0; // EOF
        let maybe_chunk = if flush {
            accumulator.flush_take_chunk()
        } else {
            accumulator.try_take_chunk()
        };
        let Some(chunk) = maybe_chunk else {
            if total_read == 0 {
                break; // EOF and nothing to flush
            }
            continue; // not enough data yet, keep reading
        };

        let raw_bytes = chunk.len();

        // Process the owned chunk. No borrow on accumulator — we own the data.
        let (output_bytes, compress_ns) = process_chunk(
            &chunk,
            &config.mode,
            &mut compressor,
            observed_time_ns,
            &mut stats,
        )?;

        stats.total_raw_bytes += raw_bytes as u64;
        stats.total_output_bytes += output_bytes as u64;
        stats.total_chunks += 1;

        // Feed the tuner.
        if let Some(ref mut tuner) = tuner {
            let chunk_stats = ChunkStats {
                raw_bytes,
                read_ns,
                compress_ns,
                compressed_bytes: output_bytes,
            };
            if let Some(tune_result) = tuner.observe(&chunk_stats) {
                if tune_result.changed {
                    accumulator.set_target_size(tune_result.recommended_size);
                }
            }
        }

        // Return the buffer for reuse.
        accumulator.reclaim(chunk);
    }

    stats.elapsed_ns = start.elapsed().as_nanos() as u64;
    stats.final_chunk_size = tuner
        .as_ref()
        .map(|t| t.best_size())
        .unwrap_or(config.chunk.target_size);
    stats.tuner_scores = tuner
        .as_ref()
        .map(|t| t.all_scores())
        .unwrap_or_default();

    Ok(stats)
}

/// Process a single owned chunk according to the output mode.
/// Returns (output_bytes, processing_ns).
fn process_chunk(
    chunk: &OwnedChunk,
    mode: &OutputMode,
    compressor: &mut ChunkCompressor,
    observed_time_ns: u64,
    stats: &mut PipelineStats,
) -> io::Result<(usize, u64)> {
    let data = chunk.data();

    match mode {
        OutputMode::Passthrough => {
            let line_count = memchr::memchr_iter(b'\n', data).count();
            stats.total_lines += line_count as u64;
            Ok((data.len(), 0))
        }
        OutputMode::RawChunk => {
            let line_count = memchr::memchr_iter(b'\n', data).count();
            stats.total_lines += line_count as u64;

            let compress_start = Instant::now();
            let compressed = compressor.compress(data)?;
            let ns = compress_start.elapsed().as_nanos() as u64;
            Ok((compressed.compressed_size as usize, ns))
        }
        OutputMode::Otlp => {
            // Split into lines, encode as OTLP protobuf, compress.
            let mut line_refs: Vec<&[u8]> = Vec::new();
            let mut line_start = 0;
            for pos in memchr::memchr_iter(b'\n', data) {
                let line = &data[line_start..pos];
                if !line.is_empty() {
                    line_refs.push(line);
                }
                line_start = pos + 1;
            }
            if line_start < data.len() {
                line_refs.push(&data[line_start..]);
            }

            stats.total_lines += line_refs.len() as u64;

            let encode_start = Instant::now();
            let otlp_bytes = otlp::encode_batch(&line_refs, observed_time_ns);
            let encode_ns = encode_start.elapsed().as_nanos() as u64;

            let compress_start = Instant::now();
            let compressed = compressor.compress(&otlp_bytes)?;
            let compress_ns = compress_start.elapsed().as_nanos() as u64;

            Ok((compressed.compressed_size as usize, encode_ns + compress_ns))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn make_log_data(num_lines: usize) -> Vec<u8> {
        let line = "2024-01-15T10:30:00Z INFO  service handled request path=/api/v1/users status=200 duration_ms=3 request_id=abc123def456\n";
        line.as_bytes().repeat(num_lines)
    }

    fn make_json_log_data(num_lines: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(num_lines * 200);
        for i in 0..num_lines {
            let level = ["INFO", "DEBUG", "WARN", "ERROR"][i % 4];
            let line = format!(
                r#"{{"timestamp":"2024-01-15T10:30:00.{:03}Z","level":"{}","message":"request handled","path":"/api/v1/users/{}"}}"#,
                i % 1000, level, 10000 + i % 90000,
            );
            data.extend_from_slice(line.as_bytes());
            data.push(b'\n');
        }
        data
    }

    #[test]
    fn test_raw_chunk() {
        let data = make_log_data(10_000);
        let mut cursor = Cursor::new(&data);
        let config = PipelineConfig {
            adaptive: false,
            mode: OutputMode::RawChunk,
            ..Default::default()
        };
        let stats = run_reader(&mut cursor, config).unwrap();
        assert_eq!(stats.total_lines, 10_000);
        assert!(stats.avg_ratio() > 1.0);
    }

    #[test]
    fn test_otlp() {
        let data = make_json_log_data(10_000);
        let mut cursor = Cursor::new(&data);
        let config = PipelineConfig {
            adaptive: false,
            mode: OutputMode::Otlp,
            ..Default::default()
        };
        let stats = run_reader(&mut cursor, config).unwrap();
        assert_eq!(stats.total_lines, 10_000);
        assert!(stats.total_output_bytes > 0);
    }

    #[test]
    fn test_passthrough() {
        let data = make_log_data(10_000);
        let mut cursor = Cursor::new(&data);
        let config = PipelineConfig {
            adaptive: false,
            mode: OutputMode::Passthrough,
            ..Default::default()
        };
        let stats = run_reader(&mut cursor, config).unwrap();
        assert_eq!(stats.total_lines, 10_000);
        assert_eq!(stats.total_output_bytes, stats.total_raw_bytes);
    }

    #[test]
    fn test_empty() {
        let mut cursor = Cursor::new(&b""[..]);
        let config = PipelineConfig::default();
        let stats = run_reader(&mut cursor, config).unwrap();
        assert_eq!(stats.total_lines, 0);
    }

    #[test]
    fn test_single_line() {
        let mut cursor = Cursor::new(&b"hello world\n"[..]);
        let config = PipelineConfig {
            chunk: ChunkConfig {
                target_size: 1024,
                min_size: 32,
                max_size: 4096,
                flush_timeout_us: 0,
            },
            adaptive: false,
            compression_level: 1,
            mode: OutputMode::RawChunk,
        };
        let stats = run_reader(&mut cursor, config).unwrap();
        assert_eq!(stats.total_lines, 1);
    }
}
