//! Memory usage benchmark for logfwd.
//!
//! Measures RSS (Resident Set Size) at different event-per-second (EPS) rates,
//! with and without SQL transforms.  Assumes a 1-second flush interval, so
//! batch_size == EPS.
//!
//! Usage (run in release mode for realistic numbers):
//!   cargo run --bin mem-bench -p logfwd-bench --release

use std::fmt::Write as _;

use logfwd_core::scan_config::ScanConfig;
use logfwd_core::scanner::SimdScanner;
use logfwd_transform::SqlTransform;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Event rates to benchmark.  Each rate equals the batch size for a 1 s flush.
const EPS_LEVELS: &[usize] = &[1, 10, 100, 1_000, 10_000, 100_000];

/// Iterations to run before taking measurements (warms up caches and allocators).
const WARMUP_ITERS: usize = 5;

/// Iterations used to find the peak RSS during a measurement run.
const MEASURE_ITERS: usize = 10;

// ---------------------------------------------------------------------------
// Memory reading
// ---------------------------------------------------------------------------

/// Return the process RSS in KiB from `/proc/self/status`.
/// Returns 0 on non-Linux platforms (measurements will show all zeros).
fn rss_kb() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                return rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    0
}

// ---------------------------------------------------------------------------
// Test-data generator (mirrors the pattern used in pipeline.rs benches)
// ---------------------------------------------------------------------------

/// Generate `n` newline-delimited JSON log lines (~250 bytes each).
fn gen_json_lines(n: usize) -> Vec<u8> {
    let levels = ["INFO", "ERROR", "DEBUG", "WARN"];
    let paths = [
        "/api/users",
        "/api/orders",
        "/api/health",
        "/api/auth/login",
    ];
    let mut s = String::with_capacity(n * 260);
    for i in 0..n {
        let _ = write!(
            s,
            r#"{{"timestamp":"2024-01-15T10:30:{:02}.{:09}Z","level":"{}","message":"GET {} HTTP/1.1","status":{},"duration_ms":{},"request_id":"req-{:08x}","service":"api-gateway"}}"#,
            i % 60,
            i % 1_000_000_000,
            levels[i % levels.len()],
            paths[i % paths.len()],
            [200, 200, 200, 500, 404][i % 5],
            (i % 500) + 1,
            i,
        );
        s.push('\n');
    }
    s.into_bytes()
}

// ---------------------------------------------------------------------------
// Measurement helpers
// ---------------------------------------------------------------------------

/// Per-EPS measurement result.
struct MemResult {
    eps: usize,
    /// RSS before the measurement loop begins.
    baseline_kb: u64,
    /// Highest RSS observed during the measurement loop.
    peak_kb: u64,
    /// RSS after the measurement loop (checks for retained memory).
    after_kb: u64,
    /// Total bytes of input data processed per batch.
    batch_bytes: usize,
}

/// Run the pipeline `WARMUP_ITERS + MEASURE_ITERS` times and return memory stats.
///
/// * `data`      – pre-generated log lines for this EPS level (avoids gen cost in loop)
/// * `transform` – optional pre-created [`SqlTransform`] (pass `None` for pass-through)
fn measure(data: &[u8], mut transform: Option<&mut SqlTransform>) -> (u64, u64, u64) {
    // Warmup: reach allocator steady state before taking any readings.
    for _ in 0..WARMUP_ITERS {
        let mut scanner = SimdScanner::new(ScanConfig::default());
        let batch = scanner.scan(data).expect("scan should not fail");
        if let Some(ref mut t) = transform {
            let _out = t
                .execute_blocking(batch)
                .expect("transform should not fail");
        } else {
            drop(batch);
        }
    }

    let baseline_kb = rss_kb();
    let mut peak_kb = baseline_kb;

    // Measurement loop: track peak RSS while processing batches.
    for _ in 0..MEASURE_ITERS {
        let mut scanner = SimdScanner::new(ScanConfig::default());
        let batch = scanner.scan(data).expect("scan should not fail");

        // Sample after scan (Arrow arrays allocated).
        let mid = rss_kb();
        if mid > peak_kb {
            peak_kb = mid;
        }

        if let Some(ref mut t) = transform {
            let out = t
                .execute_blocking(batch)
                .expect("transform should not fail");
            // Sample after transform (DataFusion output arrays allocated).
            let post = rss_kb();
            if post > peak_kb {
                peak_kb = post;
            }
            drop(out);
        } else {
            drop(batch);
        }
    }

    let after_kb = rss_kb();
    (baseline_kb, peak_kb, after_kb)
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn fmt_kb(kb: u64) -> String {
    if kb >= 1_024 * 1_024 {
        format!("{:.1} GiB", kb as f64 / (1_024.0 * 1_024.0))
    } else if kb >= 1_024 {
        format!("{:.1} MiB", kb as f64 / 1_024.0)
    } else {
        format!("{kb} KiB")
    }
}

fn fmt_bytes(b: usize) -> String {
    if b >= 1_024 * 1_024 {
        format!("{:.1} MiB", b as f64 / (1_024.0 * 1_024.0))
    } else if b >= 1_024 {
        format!("{:.1} KiB", b as f64 / 1_024.0)
    } else {
        format!("{b} B")
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    println!("logfwd memory benchmark");
    println!("=======================");
    println!();
    println!("Each EPS level uses batch_size = EPS (1-second flush interval assumption).");
    println!("RSS is read from /proc/self/status (Linux only).");
    println!("Run in --release mode for realistic allocator behaviour.");
    println!();

    // Pre-generate all data buffers so generation cost is excluded from measurements.
    let datasets: Vec<Vec<u8>> = EPS_LEVELS.iter().map(|&n| gen_json_lines(n)).collect();

    // -------------------------------------------------------------------------
    // Section 1: no transform — scan → drop
    // -------------------------------------------------------------------------
    println!("## Without SQL transform  (scan → discard)\n");
    let header = format!(
        "{:<10}  {:>10}  {:>14}  {:>14}  {:>12}  {:>14}",
        "EPS", "Input", "Baseline RSS", "Peak RSS", "Δ Peak", "After RSS"
    );
    println!("{header}");
    println!("{}", "─".repeat(header.len()));

    let mut no_transform: Vec<MemResult> = Vec::with_capacity(EPS_LEVELS.len());
    for (&eps, data) in EPS_LEVELS.iter().zip(datasets.iter()) {
        let (baseline_kb, peak_kb, after_kb) = measure(data, None);
        no_transform.push(MemResult {
            eps,
            baseline_kb,
            peak_kb,
            after_kb,
            batch_bytes: data.len(),
        });
    }

    for r in &no_transform {
        let delta = r.peak_kb.saturating_sub(r.baseline_kb);
        println!(
            "{:<10}  {:>10}  {:>14}  {:>14}  {:>12}  {:>14}",
            r.eps,
            fmt_bytes(r.batch_bytes),
            fmt_kb(r.baseline_kb),
            fmt_kb(r.peak_kb),
            fmt_kb(delta),
            fmt_kb(r.after_kb),
        );
    }
    println!();

    // -------------------------------------------------------------------------
    // Section 2: with SELECT * transform — scan → SELECT * → drop
    // -------------------------------------------------------------------------
    println!("## With SQL transform  (scan → SELECT * FROM logs → discard)\n");
    println!("{header}");
    println!("{}", "─".repeat(header.len()));

    let mut with_transform: Vec<MemResult> = Vec::with_capacity(EPS_LEVELS.len());
    for (&eps, data) in EPS_LEVELS.iter().zip(datasets.iter()) {
        // Create the transform once per EPS level; reuse across warmup + measure iterations.
        let mut t = SqlTransform::new("SELECT * FROM logs").expect("valid SQL");
        let (baseline_kb, peak_kb, after_kb) = measure(data, Some(&mut t));
        with_transform.push(MemResult {
            eps,
            baseline_kb,
            peak_kb,
            after_kb,
            batch_bytes: data.len(),
        });
    }

    for r in &with_transform {
        let delta = r.peak_kb.saturating_sub(r.baseline_kb);
        println!(
            "{:<10}  {:>10}  {:>14}  {:>14}  {:>12}  {:>14}",
            r.eps,
            fmt_bytes(r.batch_bytes),
            fmt_kb(r.baseline_kb),
            fmt_kb(r.peak_kb),
            fmt_kb(delta),
            fmt_kb(r.after_kb),
        );
    }
    println!();

    // -------------------------------------------------------------------------
    // Section 3: with WHERE filter transform — scan → WHERE → drop
    // -------------------------------------------------------------------------
    println!(
        "## With SQL filter  (scan → SELECT * FROM logs WHERE level_str = 'ERROR' → discard)\n"
    );
    println!("{header}");
    println!("{}", "─".repeat(header.len()));

    let mut with_filter: Vec<MemResult> = Vec::with_capacity(EPS_LEVELS.len());
    for (&eps, data) in EPS_LEVELS.iter().zip(datasets.iter()) {
        let mut t =
            SqlTransform::new("SELECT * FROM logs WHERE level_str = 'ERROR'").expect("valid SQL");
        let (baseline_kb, peak_kb, after_kb) = measure(data, Some(&mut t));
        with_filter.push(MemResult {
            eps,
            baseline_kb,
            peak_kb,
            after_kb,
            batch_bytes: data.len(),
        });
    }

    for r in &with_filter {
        let delta = r.peak_kb.saturating_sub(r.baseline_kb);
        println!(
            "{:<10}  {:>10}  {:>14}  {:>14}  {:>12}  {:>14}",
            r.eps,
            fmt_bytes(r.batch_bytes),
            fmt_kb(r.baseline_kb),
            fmt_kb(r.peak_kb),
            fmt_kb(delta),
            fmt_kb(r.after_kb),
        );
    }
    println!();

    // -------------------------------------------------------------------------
    // Section 4: overhead comparison table
    // -------------------------------------------------------------------------
    println!("## Transform memory overhead vs no transform\n");
    let cmp_header = format!(
        "{:<10}  {:>14}  {:>18}  {:>14}  {:>18}  {:>14}",
        "EPS",
        "No-transform peak",
        "SELECT * peak",
        "SELECT * overhead",
        "WHERE filter peak",
        "WHERE overhead",
    );
    println!("{cmp_header}");
    println!("{}", "─".repeat(cmp_header.len()));

    for ((no_t, sel), fil) in no_transform
        .iter()
        .zip(with_transform.iter())
        .zip(with_filter.iter())
    {
        let sel_over = sel.peak_kb.saturating_sub(no_t.peak_kb);
        let fil_over = fil.peak_kb.saturating_sub(no_t.peak_kb);

        let sel_pct = if no_t.peak_kb > 0 {
            format!(" ({:.0}%)", sel_over as f64 / no_t.peak_kb as f64 * 100.0)
        } else {
            String::new()
        };
        let fil_pct = if no_t.peak_kb > 0 {
            format!(" ({:.0}%)", fil_over as f64 / no_t.peak_kb as f64 * 100.0)
        } else {
            String::new()
        };

        println!(
            "{:<10}  {:>14}  {:>18}  {:>14}  {:>18}  {:>14}",
            no_t.eps,
            fmt_kb(no_t.peak_kb),
            fmt_kb(sel.peak_kb),
            format!("{}{}", fmt_kb(sel_over), sel_pct),
            fmt_kb(fil.peak_kb),
            format!("{}{}", fmt_kb(fil_over), fil_pct),
        );
    }
    println!();

    // -------------------------------------------------------------------------
    // Footer
    // -------------------------------------------------------------------------
    println!("Notes:");
    println!("  RSS  = Resident Set Size (physical memory pages currently mapped).");
    println!("  Δ Peak = peak RSS minus baseline RSS (memory attributed to the batch).");
    println!("  Baseline RSS includes allocator bookkeeping and static data.");
    println!("  All measurements made after {WARMUP_ITERS} warmup iterations.");
    println!("  Peak is the maximum RSS observed over {MEASURE_ITERS} measurement iterations.");
}
