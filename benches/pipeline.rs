use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::io::Cursor;

use logfwd::chunk::ChunkConfig;
use logfwd::pipeline::{self, PipelineConfig};

fn make_log_data(num_lines: usize) -> Vec<u8> {
    let lines = [
        "2024-01-15T10:30:00.123Z INFO  service.handler processed request path=/api/v1/users/42 status=200 duration_ms=3 request_id=abcdef0123456789\n",
        "2024-01-15T10:30:00.124Z DEBUG service.db query completed table=users rows=42 duration_ms=5 connection_id=17\n",
        "2024-01-15T10:30:00.125Z WARN  service.pool connection pool utilization high current=85 max=100 wait_ms=12\n",
        "2024-01-15T10:30:00.126Z INFO  service.auth token validated user_id=99001 scope=read,write ip=10.0.42.17\n",
        "2024-01-15T10:30:00.127Z ERROR service.handler request failed path=/api/v1/orders/88 error=\"timeout after 500ms\" trace_id=fedcba9876543210\n",
    ];
    let mut data = Vec::with_capacity(num_lines * 140);
    for i in 0..num_lines {
        data.extend_from_slice(lines[i % lines.len()].as_bytes());
    }
    data
}

fn bench_chunk_sizes(c: &mut Criterion) {
    let data = make_log_data(1_000_000); // ~140MB
    let data_len = data.len();

    let mut group = c.benchmark_group("chunk_pipeline");
    group.throughput(Throughput::Bytes(data_len as u64));
    group.sample_size(10);

    for chunk_kb in [32, 64, 128, 256, 512, 1024] {
        group.bench_with_input(
            BenchmarkId::new("chunk_size_kb", chunk_kb),
            &chunk_kb,
            |b, &chunk_kb| {
                b.iter(|| {
                    let mut cursor = Cursor::new(&data);
                    let config = PipelineConfig {
                        chunk: ChunkConfig {
                            target_size: chunk_kb * 1024,
                            min_size: 32 * 1024,
                            max_size: 2 * 1024 * 1024,
                            flush_timeout_us: 0,
                        },
                        compression_level: 1,
                        adaptive: false,
                        mode: logfwd::pipeline::OutputMode::RawChunk,
                    };
                    pipeline::run_reader(&mut cursor, config).unwrap()
                });
            },
        );
    }
    group.finish();
}

fn bench_adaptive(c: &mut Criterion) {
    let data = make_log_data(1_000_000);
    let data_len = data.len();

    let mut group = c.benchmark_group("adaptive_pipeline");
    group.throughput(Throughput::Bytes(data_len as u64));
    group.sample_size(10);

    group.bench_function("adaptive_on", |b| {
        b.iter(|| {
            let mut cursor = Cursor::new(&data);
            let config = PipelineConfig {
                adaptive: true,
                ..Default::default()
            };
            pipeline::run_reader(&mut cursor, config).unwrap()
        });
    });

    group.bench_function("adaptive_off_256k", |b| {
        b.iter(|| {
            let mut cursor = Cursor::new(&data);
            let config = PipelineConfig {
                adaptive: false,
                ..Default::default()
            };
            pipeline::run_reader(&mut cursor, config).unwrap()
        });
    });

    group.finish();
}

fn make_json_log_lines(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let level = ["INFO", "DEBUG", "WARN", "ERROR"][i % 4];
            format!(
                r#"{{"timestamp":"2024-01-15T10:30:00.{:03}Z","level":"{}","message":"service handled request path=/api/v1/users/{} status=200 duration_ms={} request_id={:016x}","service":"myapp"}}"#,
                i % 1000,
                level,
                10000 + (i * 7) % 90000,
                1 + (i * 13) % 500,
                (i as u64).wrapping_mul(0x517cc1b727220a95),
            )
            .into_bytes()
        })
        .collect()
}

fn bench_otlp_encode(c: &mut Criterion) {
    let lines = make_json_log_lines(10_000);
    let total_bytes: usize = lines.iter().map(|l| l.len()).sum();

    let mut group = c.benchmark_group("otlp_encode");
    group.throughput(Throughput::Elements(lines.len() as u64));
    group.sample_size(20);

    // Per-record with fresh buffer each time (worst case)
    group.bench_function("per_record_fresh_buf", |b| {
        b.iter(|| {
            let mut buf = Vec::with_capacity(total_bytes * 2);
            let now = 1705314600_000_000_000u64;
            for line in &lines {
                logfwd::otlp::encode_log_record(line, now, &mut buf);
            }
            buf.len()
        });
    });

    // Per-record with reused buffer (measures pure encode cost)
    group.bench_function("per_record_reused_buf", |b| {
        let mut buf = Vec::with_capacity(total_bytes * 2);
        let now = 1705314600_000_000_000u64;
        b.iter(|| {
            buf.clear();
            for line in &lines {
                logfwd::otlp::encode_log_record(line, now, &mut buf);
            }
            buf.len()
        });
    });

    // Full batch with reusable encoder
    group.bench_function("batch_reusable_encoder", |b| {
        let refs: Vec<&[u8]> = lines.iter().map(|l| l.as_slice()).collect();
        let mut encoder = logfwd::otlp::BatchEncoder::new();
        let now = 1705314600_000_000_000u64;
        b.iter(|| {
            encoder.encode(&refs, now)
        });
    });

    // encode_from_buf (takes contiguous newline-delimited bytes)
    group.bench_function("encode_from_buf", |b| {
        // Build a contiguous buffer of newline-delimited lines
        let mut contiguous = Vec::with_capacity(total_bytes + lines.len());
        for line in &lines {
            contiguous.extend_from_slice(line);
            contiguous.push(b'\n');
        }
        let mut encoder = logfwd::otlp::BatchEncoder::new();
        let now = 1705314600_000_000_000u64;
        b.iter(|| {
            encoder.encode_from_buf(&contiguous, now)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_chunk_sizes, bench_adaptive, bench_otlp_encode);
criterion_main!(benches);
