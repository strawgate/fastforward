//! Auto-tuner for read buffer size. Benchmarks multiple sizes concurrently
//! during the first N reads, then locks in the best.

use std::time::Instant;

/// Candidate read buffer sizes to test.
const CANDIDATES: &[usize] = &[
    32 * 1024,    // 32KB
    64 * 1024,    // 64KB
    128 * 1024,   // 128KB
    256 * 1024,   // 256KB
    512 * 1024,   // 512KB
    1024 * 1024,  // 1MB
    2 * 1024 * 1024, // 2MB
    4 * 1024 * 1024, // 4MB
];

/// Stats for one candidate size.
struct CandidateStats {
    size: usize,
    total_bytes: u64,
    total_ns: u64,
    reads: u32,
}

impl CandidateStats {
    fn throughput_bps(&self) -> f64 {
        if self.total_ns == 0 {
            return 0.0;
        }
        self.total_bytes as f64 / (self.total_ns as f64 / 1_000_000_000.0)
    }
}

/// Read buffer auto-tuner.
pub struct ReadTuner {
    candidates: Vec<CandidateStats>,
    /// Index into candidates we're currently testing.
    current_idx: usize,
    /// How many reads per candidate before moving to the next.
    reads_per_candidate: u32,
    /// Whether tuning is complete.
    settled: bool,
    /// The winning buffer size.
    best_size: usize,
    /// The allocated buffer (resized as we test different sizes).
    buf: Vec<u8>,
}

impl ReadTuner {
    pub fn new() -> Self {
        let candidates: Vec<CandidateStats> = CANDIDATES
            .iter()
            .map(|&size| CandidateStats {
                size,
                total_bytes: 0,
                total_ns: 0,
                reads: 0,
            })
            .collect();

        // Start with the first candidate's size.
        let initial_size = candidates[0].size;

        ReadTuner {
            candidates,
            current_idx: 0,
            reads_per_candidate: 64, // 64 reads per size should be enough
            settled: false,
            best_size: 1024 * 1024, // default fallback: 1MB
            buf: vec![0u8; *CANDIDATES.last().unwrap()], // allocate for largest
        }
    }

    /// Get the full internal buffer (for accessing data after read).
    /// Use this with `&buf()[..n]` where `n` is the return value from `read()`.
    pub fn buf(&self) -> &[u8] {
        &self.buf
    }

    /// Get the current read buffer as a mutable slice (for read() calls).
    /// The size changes during tuning, then stays fixed after settling.
    pub fn buf_mut(&mut self) -> &mut [u8] {
        let size = if self.settled {
            self.best_size
        } else {
            self.candidates[self.current_idx].size
        };
        &mut self.buf[..size]
    }

    /// Record a read result. Call this after each `file.read()` with the
    /// elapsed time and bytes read.
    pub fn record(&mut self, bytes_read: usize, elapsed_ns: u64) {
        if self.settled {
            return;
        }

        // Only count reads that returned data (skip EOF reads).
        if bytes_read == 0 {
            return;
        }

        let stats = &mut self.candidates[self.current_idx];
        stats.total_bytes += bytes_read as u64;
        stats.total_ns += elapsed_ns;
        stats.reads += 1;

        // Move to next candidate after enough reads.
        if stats.reads >= self.reads_per_candidate {
            self.current_idx += 1;
            if self.current_idx >= self.candidates.len() {
                self.settle();
            }
        }
    }

    /// Pick the winner and lock in.
    fn settle(&mut self) {
        let best = self
            .candidates
            .iter()
            .filter(|c| c.reads > 0)
            .max_by(|a, b| {
                a.throughput_bps()
                    .partial_cmp(&b.throughput_bps())
                    .unwrap()
            });

        if let Some(winner) = best {
            self.best_size = winner.size;
        }

        self.settled = true;

        eprintln!("  [read_tuner] results:");
        for c in &self.candidates {
            if c.reads == 0 {
                continue;
            }
            let marker = if c.size == self.best_size {
                " <-- best"
            } else {
                ""
            };
            eprintln!(
                "    {:>6}KB: {:.0} MB/s ({} reads, {} bytes){marker}",
                c.size / 1024,
                c.throughput_bps() / (1024.0 * 1024.0),
                c.reads,
                c.total_bytes,
            );
        }
    }

    /// Whether tuning is complete.
    pub fn is_settled(&self) -> bool {
        self.settled
    }

    /// The current best (or settled) buffer size.
    pub fn best_size(&self) -> usize {
        self.best_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tuner_settles() {
        let mut tuner = ReadTuner::new();

        // Simulate reads at each size.
        for _ in 0..10000 {
            if tuner.is_settled() {
                break;
            }
            let buf_size = tuner.buf_mut().len();
            // Simulate: larger buffers read more bytes in roughly the same time.
            let bytes = buf_size;
            let ns = 100_000; // 100μs per read regardless of size
            tuner.record(bytes, ns);
        }

        assert!(tuner.is_settled());
        // Largest size should win since throughput = bytes/time and time is constant.
        assert_eq!(tuner.best_size(), *CANDIDATES.last().unwrap());
    }

    #[test]
    fn test_tuner_picks_fastest() {
        let mut tuner = ReadTuner::new();

        for _ in 0..10000 {
            if tuner.is_settled() {
                break;
            }
            let buf_size = tuner.buf_mut().len();
            let bytes = buf_size;
            // Simulate: 256KB has the fastest throughput (lowest ns per byte).
            let ns_per_byte = if buf_size == 256 * 1024 { 1 } else { 3 };
            let ns = (bytes as u64) * ns_per_byte;
            tuner.record(bytes, ns);
        }

        assert!(tuner.is_settled());
        assert_eq!(tuner.best_size(), 256 * 1024);
    }
}
