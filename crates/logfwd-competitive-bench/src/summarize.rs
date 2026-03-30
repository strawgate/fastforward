//! Aggregate results from matrix CI cells into summary reports.
//!
//! Reads JSONL result files from `--results-dir` (one per matrix cell),
//! computes averages and standard deviation, and outputs:
//! - Markdown summary table (to stdout)
//! - github-action-benchmark JSON (optional, via --gh-bench-file)

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;

use crate::runner::BenchResult;

/// Key for grouping results: (agent, scenario, mode).
type GroupKey = (String, String, String);

struct AggResult {
    name: String,
    scenario: String,
    mode: String,
    runs: Vec<BenchResult>,
}

impl AggResult {
    fn avg_elapsed_ms(&self) -> u64 {
        let valid: Vec<u64> = self
            .runs
            .iter()
            .filter(|r| r.elapsed_ms > 0)
            .map(|r| r.elapsed_ms)
            .collect();
        if valid.is_empty() {
            return 0;
        }
        valid.iter().sum::<u64>() / valid.len() as u64
    }

    fn avg_lines_done(&self) -> u64 {
        let valid: Vec<u64> = self
            .runs
            .iter()
            .filter(|r| r.lines_done > 0)
            .map(|r| r.lines_done)
            .collect();
        if valid.is_empty() {
            return 0;
        }
        valid.iter().sum::<u64>() / valid.len() as u64
    }

    fn stddev_elapsed_ms(&self) -> f64 {
        let valid: Vec<f64> = self
            .runs
            .iter()
            .filter(|r| r.elapsed_ms > 0)
            .map(|r| r.elapsed_ms as f64)
            .collect();
        if valid.len() < 2 {
            return 0.0;
        }
        let mean = valid.iter().sum::<f64>() / valid.len() as f64;
        let variance =
            valid.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (valid.len() - 1) as f64;
        variance.sqrt()
    }

    fn individual_elapsed(&self) -> String {
        self.runs
            .iter()
            .map(|r| r.elapsed_ms.to_string())
            .collect::<Vec<_>>()
            .join("|")
    }
}

/// Load all JSONL result files from a directory tree.
/// Expects files named `results-*.jsonl` or any `.jsonl` file in subdirectories.
fn load_results(dir: &Path) -> Vec<BenchResult> {
    let mut results = Vec::new();

    for path in walkdir(dir) {
        if path.extension().is_some_and(|ext| ext == "jsonl")
            && let Ok(file) = std::fs::File::open(&path)
        {
            for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
                if let Ok(result) = serde_json::from_str::<BenchResult>(&line) {
                    results.push(result);
                }
            }
        }
    }

    results
}

/// Simple recursive directory walk (avoids adding walkdir crate).
fn walkdir(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir(&path));
            } else {
                files.push(path);
            }
        }
    }
    files
}

/// Group results by (agent, scenario, mode).
fn group_results(results: Vec<BenchResult>) -> Vec<AggResult> {
    let mut groups: BTreeMap<GroupKey, Vec<BenchResult>> = BTreeMap::new();

    for r in results {
        let key = (
            r.name.clone(),
            r.scenario.name().to_string(),
            r.mode.clone(),
        );
        groups.entry(key).or_default().push(r);
    }

    groups
        .into_iter()
        .map(|((name, scenario, mode), runs)| AggResult {
            name,
            scenario,
            mode,
            runs,
        })
        .collect()
}

fn fmt_rate(lines: u64, ms: u64) -> String {
    if ms == 0 {
        return "N/A".to_string();
    }
    let lps = lines as f64 / (ms as f64 / 1000.0);
    if lps >= 1_000_000.0 {
        format!("{:.2}M lines/sec", lps / 1_000_000.0)
    } else if lps >= 1_000.0 {
        format!("{:.0}K lines/sec", lps / 1_000.0)
    } else {
        format!("{:.0} lines/sec", lps)
    }
}

pub fn run(results_dir: &Path, markdown: bool, gh_bench_file: Option<&Path>) {
    let results = load_results(results_dir);
    if results.is_empty() {
        eprintln!("No results found in {}", results_dir.display());
        std::process::exit(1);
    }

    eprintln!("Loaded {} individual run results", results.len());

    let groups = group_results(results);

    // Collect unique scenarios in order.
    let mut scenarios: Vec<String> = Vec::new();
    for g in &groups {
        if !scenarios.contains(&g.scenario) {
            scenarios.push(g.scenario.clone());
        }
    }

    if markdown {
        print_markdown_summary(&groups, &scenarios);
    } else {
        print_table_summary(&groups, &scenarios);
    }

    if let Some(path) = gh_bench_file {
        write_gh_bench_json(&groups, path);
    }
}

fn print_markdown_summary(groups: &[AggResult], scenarios: &[String]) {
    for scenario in scenarios {
        let scenario_groups: Vec<&AggResult> =
            groups.iter().filter(|g| g.scenario == *scenario).collect();
        if scenario_groups.is_empty() {
            continue;
        }

        let iterations = scenario_groups.first().map(|g| g.runs.len()).unwrap_or(1);
        println!("### {scenario} ({iterations} iterations)\n");
        println!("| Agent | Mode | Avg Time | Stddev | Throughput | Runs |");
        println!("|-------|------|--------:|---------:|-----------:|------|");

        for g in &scenario_groups {
            let avg_ms = g.avg_elapsed_ms();
            let stddev = g.stddev_elapsed_ms();
            let avg_lines = g.avg_lines_done();
            let rate = fmt_rate(avg_lines, avg_ms);
            let runs = g.individual_elapsed();

            if avg_ms == 0 {
                println!("| {} | {} | FAILED | - | - | {} |", g.name, g.mode, runs);
            } else {
                println!(
                    "| {} | {} | {}ms | {:.0}ms | {} | {} |",
                    g.name, g.mode, avg_ms, stddev, rate, runs,
                );
            }
        }

        // Comparison ratios.
        if scenario_groups.len() > 1 {
            let base = scenario_groups[0];
            let base_ms = base.avg_elapsed_ms();
            if base_ms > 0 {
                println!();
                for g in &scenario_groups[1..] {
                    let g_ms = g.avg_elapsed_ms();
                    if g_ms > 0 {
                        let ratio = g_ms as f64 / base_ms as f64;
                        println!(
                            "> **{}** is **{:.1}x faster** than {}",
                            base.name, ratio, g.name
                        );
                    }
                }
            }
        }
        println!();
    }
}

fn print_table_summary(groups: &[AggResult], scenarios: &[String]) {
    for scenario in scenarios {
        let scenario_groups: Vec<&AggResult> =
            groups.iter().filter(|g| g.scenario == *scenario).collect();
        if scenario_groups.is_empty() {
            continue;
        }

        let iterations = scenario_groups.first().map(|g| g.runs.len()).unwrap_or(1);
        println!("===========================================");
        println!("  {scenario} ({iterations} iterations)");
        println!("===========================================");
        println!(
            "  {:<16} {:<8} {:>10} {:>10} {:>20}",
            "Agent", "Mode", "Avg Time", "Stddev", "Throughput"
        );
        println!(
            "  {:<16} {:<8} {:>10} {:>10} {:>20}",
            "-----", "----", "--------", "------", "----------"
        );

        for g in &scenario_groups {
            let avg_ms = g.avg_elapsed_ms();
            let stddev = g.stddev_elapsed_ms();
            let avg_lines = g.avg_lines_done();
            let rate = fmt_rate(avg_lines, avg_ms);

            if avg_ms == 0 {
                println!(
                    "  {:<16} {:<8} {:>10} {:>10} {:>20}",
                    g.name, g.mode, "FAILED", "-", "-"
                );
            } else {
                println!(
                    "  {:<16} {:<8} {:>8}ms {:>8.0}ms {:>20}",
                    g.name, g.mode, avg_ms, stddev, rate
                );
            }
        }
        println!("===========================================");
        println!();
    }
}

fn write_gh_bench_json(groups: &[AggResult], path: &Path) {
    #[derive(serde::Serialize)]
    struct Entry {
        name: String,
        unit: String,
        value: f64,
        extra: String,
    }

    let entries: Vec<Entry> = groups
        .iter()
        .filter(|g| g.avg_elapsed_ms() > 0)
        .map(|g| {
            let avg_ms = g.avg_elapsed_ms();
            let avg_lines = g.avg_lines_done();
            let lps = avg_lines as f64 / (avg_ms as f64 / 1000.0);
            let stddev = g.stddev_elapsed_ms();
            let n = g.runs.len();
            Entry {
                name: format!("{}/{} ({})", g.scenario, g.name, g.mode),
                unit: "lines/sec".to_string(),
                value: lps,
                extra: format!("avg={avg_ms}ms stddev={stddev:.0}ms n={n}"),
            }
        })
        .collect();

    let json = serde_json::to_string_pretty(&entries).unwrap();
    match std::fs::write(path, json) {
        Ok(()) => eprintln!("github-action-benchmark JSON written to {}", path.display()),
        Err(e) => eprintln!("ERROR: failed to write gh-bench JSON: {e}"),
    }
}
