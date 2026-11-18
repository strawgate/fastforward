//! Prometheus text exposition → Arrow benchmarks.
//!
//! Measures the SIMD scanner on its own (no-op builder) and end to end into a
//! `RecordBatch`, on a kube-state-metrics-shaped exposition (many labels,
//! long label values) and a histogram-heavy one (`_bucket` series).

use std::fmt::Write as _;
use std::hint::black_box;

use bytes::Bytes;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ffwd_arrow::prom::PromBatchBuilder;
use ffwd_core::prom_text::{PromBuilder, PromScanConfig, PromScratch, scan_prom_text};
use ffwd_core::scanner::ScanBuilder;

struct NoopBuilder;

impl ScanBuilder for NoopBuilder {
    fn begin_row(&mut self) {}
    fn end_row(&mut self) {}
    fn resolve_field(&mut self, key: &[u8]) -> usize {
        key.len()
    }
    fn append_str_by_idx(&mut self, _: usize, value: &[u8]) {
        black_box(value);
    }
    fn append_int_by_idx(&mut self, _: usize, _: &[u8]) {}
    fn append_float_by_idx(&mut self, _: usize, _: &[u8]) {}
    fn append_bool_by_idx(&mut self, _: usize, _: bool) {}
    fn append_null_by_idx(&mut self, _: usize) {}
    fn append_line(&mut self, _: &[u8]) {}
}

impl PromBuilder for NoopBuilder {
    fn append_f64_by_idx(&mut self, _: usize, value: f64) {
        black_box(value);
    }
    fn append_i64_by_idx(&mut self, _: usize, value: i64) {
        black_box(value);
    }
}

fn kube_state_metrics(pods: usize) -> String {
    let mut s = String::from(
        "# HELP kube_pod_status_phase The pods current phase.\n# TYPE kube_pod_status_phase gauge\n",
    );
    for i in 0..pods {
        for phase in ["Pending", "Succeeded", "Failed", "Unknown", "Running"] {
            writeln!(
                s,
                "kube_pod_status_phase{{namespace=\"team-{}\",pod=\"svc-{}-6d8f9c7b5-{i:05x}\",uid=\"8c1f{i:04x}-5e2b-4d3a-9f10-2b7c{i:08x}\",phase=\"{phase}\"}} {}",
                i % 40,
                i % 300,
                u8::from(phase == "Running"),
            )
            .expect("write to String");
        }
    }
    s
}

fn histograms(routes: usize) -> String {
    let mut s = String::from("# TYPE http_request_duration_seconds histogram\n");
    for r in 0..routes {
        let lbl = format!(
            "service=\"checkout\",route=\"/api/v1/resource{r}/{{id}}\",method=\"GET\",status=\"200\""
        );
        for (i, le) in ["0.005", "0.01", "0.05", "0.1", "0.5", "1", "5", "+Inf"]
            .iter()
            .enumerate()
        {
            writeln!(
                s,
                "http_request_duration_seconds_bucket{{{lbl},le=\"{le}\"}} {}",
                i * 17 + r
            )
            .expect("write to String");
        }
        writeln!(s, "http_request_duration_seconds_sum{{{lbl}}} {}.25", r * 3)
            .expect("write to String");
        writeln!(s, "http_request_duration_seconds_count{{{lbl}}} {}", r * 9)
            .expect("write to String");
    }
    s
}

fn bench_prom_text(c: &mut Criterion) {
    let inputs = [
        ("kube_state_metrics", kube_state_metrics(4000)),
        ("histograms", histograms(1000)),
    ];
    let mut group = c.benchmark_group("prom_text");
    for (name, text) in &inputs {
        let bytes = Bytes::from(text.clone());
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        let mut scratch = PromScratch::new();
        group.bench_function(format!("{name}/scan_only"), |b| {
            b.iter(|| {
                scan_prom_text(
                    black_box(&bytes),
                    &PromScanConfig::default(),
                    &mut scratch,
                    &mut NoopBuilder,
                )
                .expect("valid exposition")
            });
        });
        let mut builder = PromBatchBuilder::default();
        group.bench_function(format!("{name}/to_arrow"), |b| {
            b.iter(|| builder.scan(bytes.clone()).expect("valid exposition"));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_prom_text);
criterion_main!(benches);
