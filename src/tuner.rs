//! Adaptive chunk size tuner. Explores a fine-grained ladder of candidate
//! sizes, measures real throughput and compression ratio at each, then
//! converges on the optimal size for the current workload and hardware.
//!
//! The approach has three phases:
//! 1. **Sweep**: Try every candidate size, collect stats for each.
//! 2. **Refine**: Zoom in around the best size with finer increments.
//! 3. **Monitor**: Stay at the best size, but periodically re-sweep
//!    to detect workload changes.

use std::time::Instant;

use crate::chunk::ChunkStats;

/// A candidate chunk size and its measured performance.
#[derive(Clone, Debug)]
struct SizeScore {
    size: usize,
    /// Average throughput * compression ratio. Higher is better.
    /// This balances raw speed against network savings.
    score: f64,
    /// Number of observations averaged into this score.
    samples: usize,
    throughput_bps: f64,
    ratio: f64,
}

/// The tuner's recommendation after observing performance.
#[derive(Clone, Debug)]
pub struct TuneResult {
    /// Recommended chunk size (bytes).
    pub recommended_size: usize,
    /// Current measured throughput (bytes/sec).
    pub throughput_bps: f64,
    /// Current compression ratio.
    pub ratio: f64,
    /// Whether the tuner changed the recommendation.
    pub changed: bool,
}

/// Phase the tuner is currently in.
#[derive(Clone, Debug, PartialEq)]
enum Phase {
    /// Testing each candidate in the ladder sequentially.
    Sweep,
    /// Zooming in with finer steps around the sweep winner.
    Refine,
    /// Locked on the best size, monitoring for workload changes.
    Monitor,
}

/// Adaptive chunk size tuner.
pub struct ChunkTuner {
    /// The full ladder of candidate sizes to explore.
    candidates: Vec<usize>,
    /// Performance scores for each candidate (parallel to `candidates`).
    scores: Vec<SizeScore>,

    /// Which candidate index we're currently testing.
    current_idx: usize,
    /// Current phase.
    phase: Phase,

    /// Observations accumulated for the current candidate.
    current_obs: Vec<(f64, f64)>, // (throughput_bps, ratio)
    /// How many chunks to observe per candidate before moving on.
    samples_per_candidate: usize,

    /// The best candidate after sweep/refine.
    best_idx: usize,

    /// Chunks processed since last re-sweep (for monitoring phase).
    chunks_since_sweep: usize,
    /// Re-sweep after this many chunks to detect workload shifts.
    re_sweep_interval: usize,

    /// Minimum allowed size.
    min_size: usize,
    /// Maximum allowed size.
    max_size: usize,
}

impl ChunkTuner {
    pub fn new(initial_size: usize, min_size: usize, max_size: usize) -> Self {
        let candidates = Self::build_ladder(min_size, max_size);
        let num_candidates = candidates.len();

        // Find the starting index closest to initial_size.
        let start_idx = candidates
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| (**s as isize - initial_size as isize).unsigned_abs())
            .map(|(i, _)| i)
            .unwrap_or(0);

        let scores = candidates
            .iter()
            .map(|&size| SizeScore {
                size,
                score: 0.0,
                samples: 0,
                throughput_bps: 0.0,
                ratio: 0.0,
            })
            .collect();

        ChunkTuner {
            candidates,
            scores,
            current_idx: 0, // start sweep from the beginning
            phase: Phase::Sweep,
            current_obs: Vec::with_capacity(64),
            samples_per_candidate: 16, // 16 chunks per candidate in sweep
            best_idx: start_idx,
            chunks_since_sweep: 0,
            re_sweep_interval: 5000, // re-sweep every 5000 chunks
            min_size,
            max_size,
        }
    }

    /// Build a ladder of candidate sizes with finer granularity.
    /// Uses powers of 2 as anchors, with 3 intermediate steps between each:
    /// 32K, 48K, 64K, 96K, 128K, 192K, 256K, 384K, 512K, 768K, 1M, 1.5M, 2M
    fn build_ladder(min_size: usize, max_size: usize) -> Vec<usize> {
        let mut sizes = Vec::new();

        // Start from 32KB, go up in a geometric progression.
        // The multipliers give us 1x, 1.5x, 2x, 3x pattern per octave.
        let base_sizes: &[usize] = &[
            32 * 1024,
            48 * 1024,
            64 * 1024,
            96 * 1024,
            128 * 1024,
            160 * 1024,
            192 * 1024,
            256 * 1024,
            320 * 1024,
            384 * 1024,
            512 * 1024,
            640 * 1024,
            768 * 1024,
            1024 * 1024,
            1280 * 1024,
            1536 * 1024,
            2048 * 1024,
            2560 * 1024,
            3072 * 1024,
            4096 * 1024,
            5120 * 1024,
            6144 * 1024,
            8192 * 1024,
            10240 * 1024,
            12288 * 1024,
            16384 * 1024,
        ];

        for &size in base_sizes {
            if size >= min_size && size <= max_size {
                sizes.push(size);
            }
        }

        // Ensure we have at least the min and max.
        if sizes.is_empty() || sizes[0] != min_size {
            sizes.insert(0, min_size);
        }
        if *sizes.last().unwrap() != max_size {
            sizes.push(max_size);
        }

        sizes.dedup();
        sizes
    }

    /// Record a chunk's performance. Returns a tuning recommendation
    /// when the tuner is ready to change size.
    pub fn observe(&mut self, stats: &ChunkStats) -> Option<TuneResult> {
        let total_ns = stats.read_ns + stats.compress_ns;
        if total_ns == 0 || stats.raw_bytes == 0 {
            return None;
        }

        let throughput_bps = stats.raw_bytes as f64 / (total_ns as f64 / 1_000_000_000.0);
        let ratio = if stats.compressed_bytes > 0 {
            stats.raw_bytes as f64 / stats.compressed_bytes as f64
        } else {
            1.0
        };

        self.current_obs.push((throughput_bps, ratio));

        match self.phase {
            Phase::Sweep => self.observe_sweep(),
            Phase::Refine => self.observe_refine(),
            Phase::Monitor => self.observe_monitor(),
        }
    }

    fn observe_sweep(&mut self) -> Option<TuneResult> {
        // Scale samples needed inversely with chunk size: large chunks need
        // fewer samples since each sample covers more data. Minimum 4 samples
        // for statistical stability, maximum samples_per_candidate.
        let current_size = self.candidates[self.current_idx];
        let base_size = 256 * 1024; // baseline: full samples at 256KB
        let needed = ((self.samples_per_candidate as f64 * base_size as f64 / current_size as f64)
            .ceil() as usize)
            .clamp(4, self.samples_per_candidate);
        if self.current_obs.len() < needed {
            return None; // need more samples at this size
        }

        // Record score for current candidate.
        self.record_current_score();

        // Move to next candidate.
        self.current_idx += 1;
        if self.current_idx < self.candidates.len() {
            // More candidates to test.
            let next_size = self.candidates[self.current_idx];
            Some(TuneResult {
                recommended_size: next_size,
                throughput_bps: self.scores[self.current_idx - 1].throughput_bps,
                ratio: self.scores[self.current_idx - 1].ratio,
                changed: true,
            })
        } else {
            // Sweep complete. Find the winner and enter refine phase.
            self.best_idx = self
                .scores
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.score.partial_cmp(&b.score).unwrap())
                .map(|(i, _)| i)
                .unwrap_or(0);

            self.enter_refine_phase()
        }
    }

    fn enter_refine_phase(&mut self) -> Option<TuneResult> {
        // Build refinement candidates: finer steps around the winner.
        let best_size = self.candidates[self.best_idx];
        let step = best_size / 8; // 12.5% steps

        // Check one step below and one step above the winner.
        // But only if we haven't already tested those exact sizes.
        let refine_below = best_size.saturating_sub(step).max(self.min_size);
        let refine_above = (best_size + step).min(self.max_size);

        // Insert refinement candidates if they're new.
        let mut new_candidates = Vec::new();
        for candidate in [refine_below, refine_above] {
            if !self.candidates.contains(&candidate) {
                new_candidates.push(candidate);
            }
        }

        if new_candidates.is_empty() {
            // Nothing new to test — go straight to monitor.
            self.phase = Phase::Monitor;
            self.chunks_since_sweep = 0;

            let best = &self.scores[self.best_idx];
            return Some(TuneResult {
                recommended_size: best.size,
                throughput_bps: best.throughput_bps,
                ratio: best.ratio,
                changed: true,
            });
        }

        // Add new candidates and their score slots.
        for &size in &new_candidates {
            let insert_pos = self.candidates.partition_point(|&s| s < size);
            self.candidates.insert(insert_pos, size);
            self.scores.insert(
                insert_pos,
                SizeScore {
                    size,
                    score: 0.0,
                    samples: 0,
                    throughput_bps: 0.0,
                    ratio: 0.0,
                },
            );
            // Adjust best_idx if we inserted before it.
            if insert_pos <= self.best_idx {
                self.best_idx += 1;
            }
        }

        // Start testing the first new candidate.
        self.phase = Phase::Refine;
        self.current_idx = self
            .candidates
            .iter()
            .position(|&s| s == new_candidates[0])
            .unwrap();
        self.current_obs.clear();

        Some(TuneResult {
            recommended_size: new_candidates[0],
            throughput_bps: 0.0,
            ratio: 0.0,
            changed: true,
        })
    }

    fn observe_refine(&mut self) -> Option<TuneResult> {
        if self.current_obs.len() < self.samples_per_candidate {
            return None;
        }

        self.record_current_score();

        // Find next untested refinement candidate.
        let next_untested = self
            .scores
            .iter()
            .enumerate()
            .find(|(_, s)| s.samples == 0)
            .map(|(i, _)| i);

        if let Some(next_idx) = next_untested {
            self.current_idx = next_idx;
            Some(TuneResult {
                recommended_size: self.candidates[next_idx],
                throughput_bps: 0.0,
                ratio: 0.0,
                changed: true,
            })
        } else {
            // All refinement candidates tested. Pick overall winner.
            self.best_idx = self
                .scores
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.score.partial_cmp(&b.score).unwrap())
                .map(|(i, _)| i)
                .unwrap_or(0);

            self.phase = Phase::Monitor;
            self.chunks_since_sweep = 0;

            let best = &self.scores[self.best_idx];
            Some(TuneResult {
                recommended_size: best.size,
                throughput_bps: best.throughput_bps,
                ratio: best.ratio,
                changed: true,
            })
        }
    }

    fn observe_monitor(&mut self) -> Option<TuneResult> {
        self.chunks_since_sweep += 1;

        // Periodically re-sweep to detect workload changes.
        if self.chunks_since_sweep >= self.re_sweep_interval {
            // Reset all scores and start a fresh sweep.
            for score in &mut self.scores {
                score.score = 0.0;
                score.samples = 0;
                score.throughput_bps = 0.0;
                score.ratio = 0.0;
            }
            self.phase = Phase::Sweep;
            self.current_idx = 0;
            self.current_obs.clear();

            return Some(TuneResult {
                recommended_size: self.candidates[0],
                throughput_bps: 0.0,
                ratio: 0.0,
                changed: true,
            });
        }

        // Discard observations in monitor mode (we're not testing).
        if self.current_obs.len() > 16 {
            self.current_obs.clear();
        }

        None
    }

    fn record_current_score(&mut self) {
        let n = self.current_obs.len() as f64;
        if n == 0.0 {
            return;
        }

        let avg_throughput = self.current_obs.iter().map(|(t, _)| t).sum::<f64>() / n;
        let avg_ratio = self.current_obs.iter().map(|(_, r)| r).sum::<f64>() / n;
        let score = avg_throughput * avg_ratio;

        let entry = &mut self.scores[self.current_idx];
        if entry.samples == 0 {
            entry.throughput_bps = avg_throughput;
            entry.ratio = avg_ratio;
            entry.score = score;
        } else {
            // Exponential moving average to blend with previous observations.
            let alpha = 0.5;
            entry.throughput_bps = entry.throughput_bps * (1.0 - alpha) + avg_throughput * alpha;
            entry.ratio = entry.ratio * (1.0 - alpha) + avg_ratio * alpha;
            entry.score = entry.throughput_bps * entry.ratio;
        }
        entry.samples += self.current_obs.len();

        self.current_obs.clear();
    }

    /// Get the current recommended chunk size.
    pub fn current_size(&self) -> usize {
        self.candidates[self.current_idx]
    }

    /// Get the best size seen so far.
    pub fn best_size(&self) -> usize {
        self.candidates[self.best_idx]
    }

    /// Get all candidate sizes and their scores (for diagnostics).
    pub fn all_scores(&self) -> Vec<(usize, f64, f64, f64)> {
        self.scores
            .iter()
            .map(|s| (s.size, s.score, s.throughput_bps, s.ratio))
            .collect()
    }

    /// Current phase name (for diagnostics).
    pub fn phase_name(&self) -> &'static str {
        match self.phase {
            Phase::Sweep => "sweep",
            Phase::Refine => "refine",
            Phase::Monitor => "monitor",
        }
    }

    /// Number of candidate sizes in the ladder.
    pub fn num_candidates(&self) -> usize {
        self.candidates.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stats(raw: usize, compressed: usize, read_ns: u64, compress_ns: u64) -> ChunkStats {
        ChunkStats {
            raw_bytes: raw,
            read_ns,
            compress_ns,
            compressed_bytes: compressed,
        }
    }

    #[test]
    fn test_ladder_generation() {
        let ladder = ChunkTuner::build_ladder(32 * 1024, 2048 * 1024);
        assert!(ladder.len() >= 10, "should have many candidates, got {}", ladder.len());
        assert_eq!(ladder[0], 32 * 1024);
        assert_eq!(*ladder.last().unwrap(), 2048 * 1024);

        // Check sorted and no duplicates.
        for w in ladder.windows(2) {
            assert!(w[0] < w[1], "not sorted: {} >= {}", w[0], w[1]);
        }
    }

    #[test]
    fn test_sweep_visits_all_candidates() {
        let mut tuner = ChunkTuner::new(256 * 1024, 32 * 1024, 2048 * 1024);
        let num_candidates = tuner.num_candidates();
        let mut sizes_visited = std::collections::HashSet::new();

        // Feed enough observations to complete the sweep.
        for _ in 0..num_candidates * 20 {
            let size = tuner.current_size();
            sizes_visited.insert(size);

            let compressed = size / 3;
            let stats = make_stats(size, compressed, 50_000, 400_000);

            if let Some(result) = tuner.observe(&stats) {
                if tuner.phase_name() != "sweep" {
                    break; // sweep complete
                }
            }
        }

        // Should have visited most candidates.
        assert!(
            sizes_visited.len() >= num_candidates - 2,
            "visited {} of {} candidates",
            sizes_visited.len(),
            num_candidates
        );
    }

    #[test]
    fn test_tuner_finds_best_size() {
        let mut tuner = ChunkTuner::new(256 * 1024, 32 * 1024, 1024 * 1024);

        // Simulate: 256KB has the best throughput * ratio product.
        // Throughput peaks sharply at 256KB then drops hard (L2 cache cliff).
        // Ratio only grows slowly with size (logarithmic).
        for _ in 0..5000 {
            let size = tuner.current_size();
            let size_kb = size as f64 / 1024.0;

            // Throughput: peaks at 256KB, drops sharply above.
            // Model a bell curve centered at 256KB in log-space.
            let log_ratio = ((size_kb).ln() - (256.0f64).ln()).abs();
            let throughput_bps = 1_500_000_000.0 * (-log_ratio * log_ratio * 4.0).exp();

            // Ratio: slow logarithmic growth (10.0 at 32KB, 14.0 at 1MB).
            let ratio = 10.0 + 4.0 * (size_kb / 32.0).ln() / (1024.0 / 32.0f64).ln();

            let raw = size;
            let compressed = (raw as f64 / ratio) as usize;
            // Derive ns from throughput: ns = bytes / (bytes_per_sec / 1e9)
            let total_ns = (raw as f64 / throughput_bps * 1_000_000_000.0) as u64;
            let read_ns = total_ns / 10;
            let compress_ns = total_ns - read_ns;

            let stats = make_stats(raw, compressed, read_ns, compress_ns);
            tuner.observe(&stats);
        }

        // Best should be near 256KB (where throughput * ratio peaks).
        let best = tuner.best_size();
        assert!(
            best >= 128 * 1024 && best <= 512 * 1024,
            "expected best near 256KB, got {} KB",
            best / 1024
        );
    }

    #[test]
    fn test_tuner_respects_bounds() {
        let mut tuner = ChunkTuner::new(64 * 1024, 32 * 1024, 128 * 1024);

        for _ in 0..2000 {
            let size = tuner.current_size();
            assert!(size >= 32 * 1024, "below min: {size}");
            assert!(size <= 128 * 1024, "above max: {size}");

            let stats = make_stats(size, size / 3, 50_000, 400_000);
            tuner.observe(&stats);
        }
    }

    #[test]
    fn test_reaches_monitor_phase() {
        let mut tuner = ChunkTuner::new(256 * 1024, 64 * 1024, 512 * 1024);

        for _ in 0..5000 {
            let size = tuner.current_size();
            let stats = make_stats(size, size / 3, 50_000, 400_000);
            tuner.observe(&stats);
        }

        assert_eq!(tuner.phase_name(), "monitor");
    }
}
