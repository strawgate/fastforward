# TODO: logfwd

## What This Is

logfwd is a high-performance log forwarder in Rust. The v2 architecture is built
and running: everything goes through Arrow RecordBatches, with DataFusion SQL
transforms and pluggable output sinks.

## Current State

```
102 tests passing, 28 source files across 6 crates, ~9,000 lines
```

### What's Built and Working

| Crate | Module | Status | Purpose |
|-------|--------|--------|---------|
| logfwd | main.rs | ✓ Working | CLI: --config, --blackhole, --generate-json |
| logfwd | pipeline.rs | ✓ Working | Unified pipeline: inputs → scan → transform → output |
| logfwd-core | scanner.rs | ✓ Complete | JSON→Arrow scanner, field pushdown |
| logfwd-core | batch_builder.rs | ✓ Complete | Typed column builders (str/int/float) |
| logfwd-core | otlp.rs | ✓ Complete | Hand-rolled OTLP protobuf encoder |
| logfwd-core | cri.rs | ✓ Complete | CRI container log parser |
| logfwd-core | compress.rs | ✓ Complete | zstd compression + wire format |
| logfwd-core | tail.rs | ✓ Complete | File tailer (notify + poll) |
| logfwd-core | diagnostics.rs | ✓ Complete | HTTP server: /health /metrics /api/pipelines / |
| logfwd-core | format.rs | ✓ Complete | FormatParser trait: CRI, JSON, Raw |
| logfwd-core | input.rs | ✓ Complete | InputSource trait + FileInput |
| logfwd-core | dashboard.html | ✓ Complete | Embedded live pipeline visualizer |
| logfwd-config | lib.rs | ✓ Complete | YAML config parser (simple + advanced) |
| logfwd-transform | lib.rs | ✓ Complete | DataFusion SQL, plan caching, schema evolution |
| logfwd-transform | udf/ | ✓ Complete | UDFs: int(), float(), regexp_extract(), grok() |
| logfwd-output | otlp_sink.rs | ✓ Complete | OTLP protobuf output from RecordBatch |
| logfwd-output | json_lines.rs | ✓ Complete | JSON lines over HTTP |
| logfwd-output | stdout.rs | ✓ Complete | Stdout output (JSON/text) |
| logfwd-output | fanout.rs | ✓ Complete | Multi-output multiplexer |
| logfwd-bench | pipeline.rs | ✓ Working | Criterion benchmarks |

### What's NOT Built Yet

1. **The SQL rewriter** — translates bare column names to typed column names
   (e.g., `duration_ms` → `COALESCE(CAST(duration_ms_int AS VARCHAR), duration_ms_str)`).
   The transform currently executes SQL directly against the RecordBatch schema —
   which uses the `{field}_{type}` naming. Users shouldn't need to write `level_str`.
   The rewriter bridges user SQL → internal SQL.

2. **Additional input sources** — only `FileInput` is implemented. Need `UdpInput`,
   `TcpInput`, and `OtlpInput` to match the config's accepted input types.

3. **Additional output sinks** — `elasticsearch`, `loki`, `parquet`, `file_out`
   are defined in config but only have placeholder modules (not wired into
   `build_output_sink`).

4. **End-to-end acks / cursor tracking** — file read offset is advanced immediately,
   not after successful delivery. Need: committed_offset (acked) vs read_offset
   (current), configurable lead (how many unacked batches allowed).

5. **Benchmarks of the full v2 path** — we have v1 benchmarks but haven't measured
   the full pipeline end-to-end: file read → format parse → scanner → DataFusion
   → output sink.

## What To Build Next (In Priority Order)

### Priority 1: The SQL Rewriter

The rewriter takes user-facing SQL and the current field type map (from the
BatchBuilder's discovered fields) and produces internal SQL that DataFusion
can execute against the typed column schema.

```rust
pub fn rewrite_sql(user_sql: &str, field_types: &FieldTypeMap) -> Result<String, String>;
```

Rewrite rules (see docs/SCANNER_AND_TRANSFORM_DESIGN.md for full details):

1. Bare column in SELECT: `duration_ms` → `COALESCE(CAST(duration_ms_int AS VARCHAR), duration_ms_str) AS duration_ms`
2. Bare column in WHERE with string literal: `level = 'ERROR'` → `level_str = 'ERROR'`
3. Bare column in WHERE with numeric literal: `status > 400` → `status_int > 400`
4. `int(x)` call: → `COALESCE(x_int, TRY_CAST(x_str AS BIGINT))`
5. `float(x)` call: → `COALESCE(CAST(x_int AS DOUBLE), x_float, TRY_CAST(x_str AS DOUBLE))`
6. `EXCEPT (field)` → `EXCEPT (field_int, field_str, field_float)`
7. `SELECT *` → expand to all fields with coalesced aliases
8. Single-type field: `level` → `level_str AS level` (no coalesce needed)

Use `sqlparser::parser::Parser` to parse the AST, walk and rewrite Expr nodes,
then re-serialize to SQL string.

**Test against these SQL patterns:**
```sql
SELECT * FROM logs
SELECT * FROM logs WHERE level = 'ERROR'
SELECT * EXCEPT (stack_trace) FROM logs
SELECT *, 'prod' AS env FROM logs
SELECT * FROM logs WHERE int(status) >= 400
SELECT AVG(float(duration_ms)) FROM logs GROUP BY service
SELECT * EXCEPT (a) REPLACE (upper(message) AS message) FROM logs
```

### Priority 2: Benchmark the v2 Path

Add benchmarks for the full pipeline:

```
Modes to benchmark:
  scan-only:          scanner → RecordBatch → discard
  scan+passthrough:   scanner → DataFusion "SELECT * FROM logs" → discard
  scan+filter:        scanner → DataFusion "WHERE level_str != 'DEBUG'" → discard
  scan+filter+otlp:   scanner → DataFusion → OtlpSink (to blackhole)
  scan+filter+json:   scanner → DataFusion → StdoutSink (to /dev/null)
  full-pipeline:      file read → CRI → scanner → DataFusion → OtlpSink
```

Performance targets:
- scan-only: ≤400ns/line (2.5M lines/sec)
- scan+passthrough: ≤500ns/line (2.0M lines/sec)
- scan+filter+otlp: ≤700ns/line (1.4M lines/sec)
- full-pipeline (no compression): ≤800ns/line (1.25M lines/sec)

### Priority 3: Wire Remaining Output Sinks

Implement `build_output_sink` support for the placeholder sinks:
- Elasticsearch (bulk API)
- Loki (push API)
- Parquet (file-based)
- FileOut (JSON/raw to file)

### Priority 4: Additional Input Sources

Implement:
- `UdpInput` — recvmmsg, syslog parsing
- `TcpInput` — newline-framed stream
- `OtlpInput` — receive OTLP over HTTP/gRPC, bypass scanner

### Priority 5: End-to-End Acks

Track committed vs read offsets so that on crash, restart resumes from the
last acknowledged position (duplicates, not loss).

## Key Design Decisions (Do Not Change)

1. **Everything goes through Arrow RecordBatch.** No fast path. No bifurcated code.
   DataFusion always runs, even for simple filters (~85ns/line overhead).

2. **Column naming: `{field}_{type}`** where type is str, int, or float.
   Multi-typed columns for fields with type conflicts.

3. **Scanner writes directly into Arrow builders.** No intermediate event struct.
   Zero per-line allocation after warmup.

4. **YAML config, SQL transforms.** Users write SQL, the system auto-detects field
   types and maps column names.

5. **Diagnostics server with embedded dashboard.** Atomic counters, no locking on
   the hot path.

## Performance Baselines (Measured)

### v1 Pipeline (raw bytes path, no Arrow)
```
VM-format data (257 bytes/line, 10 variable fields):
  jsonlines no compression:  14.5M lines/sec/core
  jsonlines + zstd:           3.1M lines/sec/core
  K8s benchmark (0.25 cores): logfwd 108K/s vs vlagent 65K/s (1.7x faster)
```

### v2 Components (research benchmarks, Rust)
```
Scanner: scan 13 fields into Arrow:         399ns/line  (2.5M lines/sec)
Scanner: scan 3 fields (pushdown):          342ns/line  (2.9M lines/sec)
Arrow column construction overhead:           93ns/line
DataFusion simple filter:                     85ns/line
DataFusion complex transform:               505ns/line
```

### v2 Targets (not yet measured end-to-end)
```
scan → DataFusion passthrough → OTLP:     ~700ns/line  (1.4M lines/sec)
scan → DataFusion filter → OTLP:          ~600ns/line  (1.7M lines/sec)
full pipeline with file read + CRI:       ~800ns/line  (1.25M lines/sec)
```

## Testing

```bash
# Run all tests
cargo test

# Run specific crate tests
cargo test -p logfwd-core
cargo test -p logfwd-transform
cargo test -p logfwd-output
cargo test -p logfwd-config

# Run criterion benchmarks
cargo bench --bench pipeline

# Generate test data
./target/release/logfwd --generate-json 5000000 /tmp/json.txt
```

## Dependencies

```toml
# Workspace-level
arrow = "54"
opentelemetry = "0.31"
opentelemetry_sdk = "0.31"
opentelemetry-otlp = "0.31"
serde = "1"

# logfwd-core
crossbeam-channel = "0.5"
memchr = "2"
notify = "7"
tiny_http = "0.12"
xxhash-rust = "0.8"
zstd = "0.13"

# logfwd-transform
datafusion = "45"
regex (via UDFs)
tokio = "1" (for DataFusion async, block_on only)

# logfwd-output
ureq = "3" (with rustls)

# logfwd (binary)
tokio = "1" (for OTel metrics exporter)
```
