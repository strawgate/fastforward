# Developing logfwd

## Codebase Overview

~4,500 lines of Rust, 45 tests, no unsafe, no async runtime. The design prioritizes throughput per core above all else.

```
src/
├── lib.rs              # Module declarations
├── main.rs             # CLI: benchmark, tail, daemon, e2e, data generation
├── chunk.rs            # Double-buffered chunk accumulator (zero-copy swap)
├── compress.rs         # zstd-1 compression with 16-byte wire format header
├── cri.rs              # CRI container log format parser + partial line reassembly
├── daemon.rs           # K8s DaemonSet mode: tail → CRI parse → inject → HTTP POST
├── e2e_bench.rs        # End-to-end benchmark harness with per-stage timing
├── otlp.rs             # Hand-rolled OTLP protobuf encoder (JSON → protobuf)
├── pipeline.rs         # Chunk-based pipeline: read → encode → compress
├── read_tuner.rs       # Auto-tunes read buffer size at startup
├── sender.rs           # HTTP sender (ureq, blocking, for jsonline endpoint)
├── tail.rs             # File tailer with inotify/kqueue + poll fallback
└── tuner.rs            # Adaptive chunk size tuner (sweep → refine → monitor)

deploy/
├── daemonset.yml       # K8s DaemonSet manifest (1 CPU, 1Gi memory limit)
└── Makefile            # Integration with VictoriaMetrics benchmark

benches/
└── pipeline.rs         # Criterion benchmarks for chunk sizes and OTLP encoding
```

## Key Design Decisions

### Double-Buffer Chunk Accumulator (`chunk.rs`)

Two heap-allocated buffers. The reader fills one while the pipeline processes the other. Handoff is a pointer swap — zero copy of the data itself. The only copy is the leftover partial line (~200 bytes avg) moved to the start of the new fill buffer after swap.

Why: Rust's borrow checker prevents holding a reference into a buffer while mutating it. The alternatives were closure-based access (limits multi-threading) or data copying (slow). Double-buffer gives us owned data that can cross thread boundaries.

### No Per-Line Allocation

The hot path allocates nothing per log line. CRI parsing, JSON field scanning, and protobuf encoding all work on references into the read buffer. The only per-batch allocations are the protobuf output buffer and the compressed output, both reused across batches via `BatchEncoder`.

Profile: 0.03 heap allocations per line (30K allocations for 1M lines — all from setup, not per-line work).

### Hand-Rolled OTLP Protobuf Encoder (`otlp.rs`)

We don't use prost or any protobuf library. The encoder writes varint tags, length prefixes, and field data directly into a `Vec<u8>`. This avoids:
- Intermediate struct allocation (prost requires owned `String` fields)
- Two-pass size computation (we do compute sizes first, but reuse buffers)
- Generic dispatch overhead

Performance: 100ns per record for full JSON field extraction + protobuf encoding. 28ns per record for raw-body mode (no JSON parsing).

The encoder supports two modes:
- `encode_log_record()`: Scans JSON for timestamp/severity/message fields, maps them to OTLP LogRecord first-class fields, puts the message as body.
- `encode_log_record_raw()`: Entire line becomes the body string. No JSON parsing. 3.5x faster.

### CRI Parser Zero-Copy Path (`cri.rs`)

`process_cri_to_buf()` writes extracted messages directly from the input buffer into an output buffer. For full lines (the common case — CRI partial lines are rare), the message bytes go straight from the read buffer to the output with no intermediate copy. Only partial line reassembly touches the reassembler's internal buffer.

### Two-Thread Daemon Architecture (`daemon.rs`)

```
Reader thread (main):
  poll files → read() → CRI parse → JSON inject → batch → push to channel

Sender threads (4x):
  receive batch from channel → HTTP POST to endpoint
```

The reader never blocks on network. If all sender threads are busy, the reader stalls on the bounded channel (backpressure). This was a critical optimization: the single-threaded version spent 92% of CPU time blocked on HTTP sends. The two-thread split brought us from 571K to 997K lines/sec.

### Adaptive Tuning

**Chunk size tuner** (`tuner.rs`): At startup, sweeps a ladder of 17+ candidate chunk sizes (32KB to 16MB). For each size, measures throughput × compression ratio. Picks the winner, then refines with finer steps around it. Re-sweeps periodically to adapt to workload changes. Found that 256KB is optimal for short lines (L2 cache sweet spot) but 5MB+ wins for long repetitive lines.

**Read buffer tuner** (`read_tuner.rs`): Tests 8 read buffer sizes (32KB to 4MB) during the first few hundred reads, picks the one with highest throughput (bytes per nanosecond of read time).

## Getting Started

### Prerequisites

The Rust toolchain is pinned via `rust-toolchain.toml` (stable, with clippy + rustfmt). Rustup will install it automatically on first `cargo` invocation.

```bash
# Required
cargo install just taplo-cli cargo-deny

# Optional (faster tests, profiling)
cargo install cargo-nextest inferno sccache
```

### Task Runner

All development tasks are available via [`just`](https://github.com/casey/just):

```bash
just --list        # Show all available recipes
just ci            # Run full CI suite locally (lint + test)
just lint          # Format check + clippy + TOML check + dependency audit
just fmt           # Auto-format Rust code
just test          # Run all tests
```

### Pre-commit Hook

```bash
just install-hooks   # Install git pre-commit hook (cargo fmt + clippy)
just uninstall-hooks # Remove it
```

The hook runs `cargo fmt --check` and `cargo clippy` before each commit (~0.3s on warm cache).

## Fast Compilation & Testing

Due to heavy dependencies like `datafusion` and `arrow`, compile times can be long. To optimize your local workflow:

1. **Test with `cargo test` (no `--release` flag):** The `[profile.test]` uses the default dev profile (opt-level 0) so tests share compiled artifacts with `cargo clippy` and `cargo check`. This avoids a full recompile when switching between linting and testing.
2. **Use `sccache`:** Caches Rust dependencies across builds.
   ```bash
   cargo install sccache
   export RUSTC_WRAPPER=sccache
   ```
3. **Use `cargo-nextest`:** Executes the test suite in parallel native processes, massively reducing test wall-clock time.
   ```bash
   just nextest
   ```

## CI

CI runs on every push and PR to `master`. It mirrors what `just ci` runs locally:

| Job | What it checks |
|-----|---------------|
| **Lint & Test (Linux)** | `cargo fmt`, `cargo clippy`, `taplo check`, `cargo deny`, typos, `cargo test`, `cargo check --release` — single job so clippy and test share compiled artifacts |
| **Test (macOS)** | `cargo test` — catches platform-specific issues (kqueue vs inotify) |
| **Actionlint** | Validates GitHub Actions workflow files |

CI uses concurrency groups with `cancel-in-progress` to avoid wasted runs on rapid pushes. Rust build cache is only saved on `master` to prevent PR branches from polluting it.

Nightly benchmarks run separately (see below) and post results as GitHub issues.

## Running Benchmarks

### Criterion Micro-Benchmarks

```bash
just bench          # Run criterion benchmarks
just bench-report   # Generate markdown summary
```

### Competitive Benchmarks

Compare logfwd against vector, fluent-bit, filebeat, otelcol-contrib, and vlagent:

```bash
# Binary mode (local dev, downloads competitors automatically)
just bench-competitive --lines 1000000

# Docker mode (fair comparison with CPU/memory limits)
just bench-competitive --lines 5000000 --docker --cpus 1 --memory 1g --markdown

# Full Docker benchmark with CPU flamegraph + memory profiling
just bench-docker
```

### Profiling

**CPU flamegraph** (Linux, requires `perf` + `inferno`):
```bash
just bench-competitive --lines 5000000 --profile ./profiles
# Produces profiles/flamegraph.svg
```

**Memory allocation profiling** (dhat-heap):
```bash
cargo build --release --features dhat-heap -p logfwd
cp target/release/logfwd target/release/logfwd-dhat
cargo build --release -p logfwd  # Rebuild clean binary

just bench-competitive --lines 5000000 --profile ./profiles --dhat-binary ./target/release/logfwd-dhat
# Produces profiles/dhat-heap.json — view at https://nnethercote.github.io/dh_view/dh_view.html
```

### Local Pipeline Benchmarks

```bash
# Generate test data
./target/release/logfwd --generate-json 5000000 /tmp/json.txt

# Pipeline benchmark (reads from file, outputs to /dev/null)
./target/release/logfwd /tmp/json.txt --mode otlp

# End-to-end with CRI parsing and per-stage timing
./target/release/logfwd --e2e --wrap-cri /tmp/json.txt /tmp/cri.txt
./target/release/logfwd --e2e /tmp/cri.txt otlp-zstd
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

### Chunk Format (compress.rs)

Used for the raw chunk pipeline. 16-byte header + zstd-compressed payload.

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

### OTLP Protobuf (otlp.rs)

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
                 └─ observed_time_unix_nano (field 11, fixed64)
```

## Profiling Results (OTLP-zstd path, 5M lines)

```
Stage           % CPU    Time     What
read             8%      85ms     file read() from page cache
cri_parse       18%     185ms     CRI format parse + JSON inject into buffer
otlp_encode     60%     622ms     JSON field scan + protobuf encoding
compress        14%     149ms     zstd level 1
```

The OTLP encoder at 60% is the bottleneck. Within that: ~60% is JSON field scanning (memchr for quotes + key matching), ~40% is protobuf writing. The raw-body mode (no JSON scan) drops OTLP encode to 14% of CPU.
