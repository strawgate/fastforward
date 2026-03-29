# Developing logfwd

## Codebase Overview

~9,000 lines of Rust across 28 source files in a 6-crate workspace, 102 tests. The design prioritizes throughput per core above all else. Tokio is used only for DataFusion's async API (via `block_on`) and the optional OTel metrics exporter — the pipeline itself is synchronous.

```
crates/
├── logfwd/                  # Binary crate: CLI entrypoint + pipeline wiring
│   └── src/
│       ├── main.rs          # CLI: --config, --blackhole, --generate-json
│       ├── pipeline.rs      # Pipeline: inputs → Scanner → SQL transform → outputs
│       └── lib.rs           # Module declarations
│
├── logfwd-core/             # Core library: parsing, encoding, tailing, diagnostics
│   └── src/
│       ├── scanner.rs       # JSON→Arrow scanner with field pushdown
│       ├── batch_builder.rs # Typed column builders (str/int/float)
│       ├── otlp.rs          # Hand-rolled OTLP protobuf encoder
│       ├── cri.rs           # CRI container log format parser
│       ├── compress.rs      # zstd-1 compression with 16-byte wire format header
│       ├── tail.rs          # File tailer with inotify/kqueue + poll fallback
│       ├── diagnostics.rs   # HTTP diagnostics server + embedded dashboard
│       ├── format.rs        # FormatParser trait: CRI, JSON, Raw parsers
│       ├── input.rs         # InputSource trait + FileInput implementation
│       ├── dashboard.html   # Embedded live pipeline visualizer
│       └── lib.rs           # Module declarations
│
├── logfwd-config/           # YAML config parsing and validation
│   └── src/lib.rs
│
├── logfwd-transform/        # SQL-based log transformation via DataFusion
│   └── src/
│       ├── lib.rs           # SqlTransform: plan caching, schema evolution
│       └── udf/             # Custom UDFs: regexp_extract, grok
│
├── logfwd-output/           # Output sinks
│   └── src/
│       ├── lib.rs           # OutputSink trait + build_output_sink factory
│       ├── otlp_sink.rs     # OTLP protobuf output (from Arrow RecordBatch)
│       ├── json_lines.rs    # JSON lines over HTTP
│       ├── stdout.rs        # Stdout (JSON or text)
│       ├── fanout.rs        # FanOut: multiplexes to multiple sinks
│       ├── elasticsearch.rs # Placeholder (not yet wired)
│       ├── loki.rs          # Placeholder (not yet wired)
│       └── parquet.rs       # Placeholder (not yet wired)
│
└── logfwd-bench/            # Criterion micro-benchmarks
    ├── benches/pipeline.rs
    └── src/main.rs          # Benchmark report generator

deploy/
├── daemonset.yml            # K8s DaemonSet manifest (1 CPU, 1Gi memory limit)
├── Makefile                 # Integration with VictoriaMetrics benchmark
└── run.sh

.github/workflows/
├── ci.yml                   # Push/PR to master: fmt, clippy, test
└── bench.yml                # Nightly: criterion benchmarks → GitHub issue
```

## Key Design Decisions

### Arrow-First Pipeline

Everything goes through Arrow RecordBatches. There is no "fast path" — DataFusion always runs, even for `SELECT * FROM logs` (~85ns/line overhead, negligible). One code path, one implementation per feature.

The pipeline flow is: **inputs → format parser → scanner → DataFusion SQL → output sinks**. Each pipeline runs on its own thread.

### No Per-Line Allocation

The hot path allocates nothing per log line. JSON scanning, Arrow column construction, and protobuf encoding all work on references into the read buffer or Arrow arrays. The only per-batch allocations are the protobuf output buffer and Arrow column builders, both reused across batches.

### Hand-Rolled OTLP Protobuf Encoder (`otlp.rs`)

We don't use prost or any protobuf library. The encoder writes varint tags, length prefixes, and field data directly into a `Vec<u8>`. This avoids:
- Intermediate struct allocation (prost requires owned `String` fields)
- Two-pass size computation (we do compute sizes first, but reuse buffers)
- Generic dispatch overhead

The encoder supports two modes:
- `encode_log_record()`: Scans JSON for timestamp/severity/message fields, maps them to OTLP LogRecord first-class fields.
- `encode_log_record_raw()`: Entire line becomes the body string. No JSON parsing. ~3.5x faster.

### CRI Parser Zero-Copy Path (`cri.rs`)

`process_cri_to_buf()` writes extracted messages directly from the input buffer into an output buffer. For full lines (the common case), the message bytes go straight from the read buffer to the output with no intermediate copy. Only partial line reassembly touches the reassembler's internal buffer.

### Typed Column Model (`batch_builder.rs`)

Each JSON field produces 1–3 Arrow columns based on observed value types: `field_str` (Utf8), `field_int` (Int64), `field_float` (Float64). When a field has consistent types, one column. When types conflict (e.g., `status=500` then `status="error"`), both `status_int` and `status_str`, with NULLs in non-matching rows. Memory overhead: ~1.03x (null bitmaps are 1 bit/row).

### SQL Transform (DataFusion)

Users write SQL. The system auto-detects field types and maps column names. Built-in UDFs: `int()`, `float()`, `regexp_extract()`, `grok()`. DataFusion plans are compiled once and cached; they recompile on schema changes (~1ms, rare).

**Note:** The SQL rewriter (translating user-facing column names like `level` to internal typed names like `level_str`) is not yet built. Currently, SQL must reference the internal `{field}_{type}` column names directly.

### Diagnostics Server (`diagnostics.rs`)

HTTP server on configurable port. Reads atomic counters — no locking on the hot path.

| Endpoint | Content |
|----------|---------|
| `GET /` | Embedded HTML dashboard with auto-refreshing pipeline visualization |
| `GET /health` | `{"status":"ok","uptime_seconds":N,"version":"0.2.0"}` |
| `GET /metrics` | Prometheus text format (counters per input/output) |
| `GET /api/pipelines` | JSON with live pipeline stats (lines/sec, errors, etc.) |

## Fast Compilation & Testing

Due to heavy dependencies like `datafusion` and `arrow`, compile times can be long. To optimize your local workflow:

1. **Test with `cargo test` (no `--release` flag):** The `[profile.test]` in `Cargo.toml` is configured to use `opt-level = 1` so tests run faster without incurring the massive compilation penalty of release's LTO and single codegen unit.
2. **Use `sccache`:** Caches Rust dependency compilation.
   ```bash
   cargo install sccache
   export RUSTC_WRAPPER=sccache
   ```
3. **Use `cargo-nextest`:** Executes the test suite in parallel native processes.
   ```bash
   cargo install cargo-nextest
   cargo nextest run
   ```

## Running Benchmarks

### Criterion Micro-Benchmarks

```bash
cargo bench --bench pipeline
```

### Allocation Profiling

```bash
cargo run --release --features dhat-heap -- --generate-json 1000000 /tmp/json.txt
# Then run with profiling:
cargo run --release --features dhat-heap -- --config your-config.yaml
# Generates dhat-heap.json, viewable at https://nnethercote.github.io/dh_view/dh_view.html
```

### VictoriaMetrics Benchmark (requires Docker + KIND)

```bash
# Clone VM benchmark repo
git clone https://github.com/VictoriaMetrics/log-collectors-benchmark /tmp/vm-bench

# Start infrastructure
cd /tmp/vm-bench && make bench-up-monitoring

# Build and deploy logfwd
docker build -t logfwd:latest .
kind load docker-image --name log-collectors-bench logfwd:latest
kubectl apply -f deploy/daemonset.yml

# Start log generators
cd /tmp/vm-bench && make bench-up-generator RAMP_UP_STEP=5 RAMP_UP_STEP_INTERVAL=1s GENERATOR_REPLICAS=10

# Check results
kubectl exec -n monitoring deploy/log-verifier -- wget -qO- http://localhost:8080/metrics | grep log_verifier_logs_total
```

## Wire Formats

### Compressed Chunk Format (`compress.rs`)

16-byte header + zstd-compressed payload.

```
Offset  Size  Field
0       2     magic (0x4C46 = "LF")
2       1     version (1)
3       1     flags (bit 0: zstd compressed)
4       4     compressed payload size (LE)
8       4     raw size (LE)
12      4     xxhash32 of compressed payload (LE)
16      N     compressed payload (newline-delimited log lines)
```

### OTLP Protobuf (`otlp.rs`)

Standard ExportLogsServiceRequest protobuf. Hand-encoded but wire-compatible with any OTLP receiver. Structure:

```
ExportLogsServiceRequest
  └─ ResourceLogs (field 1)
       └─ ScopeLogs (field 2)
            └─ repeated LogRecord (field 2)
                 ├─ time_unix_nano (field 1, fixed64)
                 ├─ severity_number (field 2, varint)
                 ├─ severity_text (field 3, string)
                 ├─ body: AnyValue { string_value } (field 5)
                 ├─ attributes: repeated KeyValue (field 6)
                 └─ observed_time_unix_nano (field 11, fixed64)
```

## CI/CD

**ci.yml** — runs on push/PR to master: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`. Uses Swatinem/rust-cache.

**bench.yml** — nightly + manual trigger: runs `cargo bench -p logfwd-bench`, generates a report, posts as a GitHub issue (auto-closes previous benchmark issues).
