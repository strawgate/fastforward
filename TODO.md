# TODO: Agent Handoff — logfwd v2 Arrow Pipeline

## What This Is

logfwd is a high-performance log forwarder in Rust. We built the v1 prototype
(file tailing → CRI parse → OTLP encode → compress → HTTP send) and validated
it at 1.7x faster than VictoriaMetrics vlagent in their own K8s benchmark.

We then designed and implemented the v2 architecture where **everything goes
through Arrow RecordBatches**. The scanner, DataFusion SQL transform, and output
sinks are all built and tested. They are NOT yet wired together into a unified
pipeline.

## Current State

```
96 tests passing, 19 source files, 8,621 lines
Branch: v2-arrow-pipeline
```

### What's Built and Working

| Module | Lines | Tests | Status | Purpose |
|--------|-------|-------|--------|---------|
| scanner.rs | 533 | 11 | ✓ Complete | JSON→Arrow scanner, field pushdown |
| batch_builder.rs | 574 | 5 | ✓ Complete | Typed column builders (str/int/float) |
| transform.rs | 691 | 10 | ✓ Complete | DataFusion SQL, int()/float() UDFs |
| output.rs | 945 | 8 | ✓ Complete | OutputSink trait, Stdout/JSON/OTLP/FanOut |
| config.rs | 633 | 12 | ✓ Complete | YAML config parser |
| diagnostics.rs | 602 | 5 | ✓ Complete | HTTP server: /health /metrics /api/pipelines |
| dashboard.html | 138 | - | ✓ Complete | Embedded live pipeline visualizer |
| cri.rs | 292 | 6 | ✓ Complete | CRI container log parser |
| otlp.rs | 726 | 10 | ✓ Complete | Hand-rolled OTLP protobuf encoder |
| chunk.rs | 307 | 5 | ✓ Complete | Double-buffer chunk accumulator |
| compress.rs | 195 | 4 | ✓ Complete | zstd compression + wire format |
| tail.rs | 618 | 5 | ✓ Complete | File tailer (notify + poll) |
| daemon.rs | 345 | 2 | ✓ Working | K8s DaemonSet mode (v1 pipeline) |
| pipeline.rs | 337 | 5 | ✓ Working | v1 chunk pipeline (for benchmarks) |
| e2e_bench.rs | 241 | - | ✓ Working | v1 E2E benchmark harness |
| tuner.rs | 570 | 5 | ✓ Working | Adaptive chunk size tuner |
| read_tuner.rs | 211 | 2 | ✓ Working | Read buffer auto-tuner |
| sender.rs | 105 | 1 | ✓ Working | HTTP sender (ureq) |
| main.rs | 541 | - | ✓ Working | CLI entrypoint |

### What's NOT Built Yet

1. **The unified v2 pipeline** — the code that wires scanner → DataFusion → output sinks
   together into a running pipeline driven by the YAML config. This is THE critical
   missing piece.

2. **Input abstraction** — currently the daemon has hardcoded file tailing. Need an
   InputSource trait so file/UDP/TCP/OTLP inputs are pluggable.

3. **The SQL rewriter** — translates bare column names to typed column names
   (e.g., `duration_ms` → `COALESCE(CAST(duration_ms_int AS VARCHAR), duration_ms_str)`).
   The transform.rs currently executes SQL directly against the RecordBatch schema —
   which uses the `{field}_{type}` naming. Users shouldn't need to write `level_str`.
   The rewriter bridges user SQL → internal SQL.

4. **End-to-end acks / cursor tracking** — file read offset is advanced immediately,
   not after successful delivery. Need: committed_offset (acked) vs read_offset
   (current), configurable lead (how many unacked batches allowed).

5. **Benchmarks of the new v2 path** — we have extensive v1 benchmarks but haven't
   measured: scanner→Arrow throughput, DataFusion passthrough cost, OTLP-from-RecordBatch
   cost, or the full v2 pipeline end-to-end.

## What To Build Next (In Priority Order)

### Priority 1: Wire the v2 Pipeline

Create `/Users/bill.easton/repos/memagent/src/pipeline_v2.rs`:

```rust
/// The v2 pipeline: config → inputs → scanner → DataFusion → outputs
pub struct Pipeline {
    name: String,
    inputs: Vec<Box<dyn InputSource>>,
    scanner_config: ScanConfig,
    transform: SqlTransform,
    outputs: FanOut,
    metrics: Arc<PipelineMetrics>,
    batch_builder: BatchBuilder,
}

impl Pipeline {
    pub fn from_config(name: &str, config: &PipelineConfig) -> Result<Self>;
    pub fn run(&mut self) -> io::Result<()>;  // blocking main loop
}
```

The run loop:
```
loop:
    for each input:
        poll() → Option<MessageBatch>
        if data:
            CRI parse (if format == cri)
            scan_json_to_batch(buf, &scan_config, &mut batch_builder)
            batch = batch_builder.finish_batch()
            result = transform.execute(batch)
            outputs.send_batch(&result, &metadata)
            update metrics
    if no data: sleep(poll_interval)
```

For the file input, reuse the existing tail.rs file tailer + cri.rs CRI parser.
The CRI output goes into a json_batch buffer, which feeds the scanner.

This should be straightforward — all the components exist, they just need plumbing.

### Priority 2: The SQL Rewriter

Create `/Users/bill.easton/repos/memagent/src/rewriter.rs`:

The rewriter takes user-facing SQL and the current field type map (from the
BatchBuilder's discovered fields) and produces internal SQL that DataFusion
can execute against the typed column schema.

```rust
pub fn rewrite_sql(user_sql: &str, field_types: &FieldTypeMap) -> Result<String, String>;
```

Rewrite rules (see docs/DATA_FUSION_PLAN_UPDATE.md for full details):

1. Bare column in SELECT: `duration_ms` → `COALESCE(CAST(duration_ms_int AS VARCHAR), duration_ms_str) AS duration_ms`
2. Bare column in WHERE with string literal: `level = 'ERROR'` → `level_str = 'ERROR'`
3. Bare column in WHERE with numeric literal: `status > 400` → `status_int > 400`
4. `int(x)` call: → `COALESCE(x_int, TRY_CAST(x_str AS BIGINT))`
5. `float(x)` call: → `COALESCE(CAST(x_int AS DOUBLE), x_float, TRY_CAST(x_str AS DOUBLE))`
6. `EXCEPT (field)` → `EXCEPT (field_int, field_str, field_float)`
7. `SELECT *` → expand to all fields with coalesced aliases
8. Single-type field: `level` → `level_str AS level` (no coalesce needed)

The FieldTypeMap is:
```rust
pub struct FieldTypeMap {
    /// field_name → which typed columns exist (e.g., "status" → [Int, Str])
    pub fields: HashMap<String, Vec<TypeTag>>,
}
```

Populate it from the BatchBuilder after finish_batch() — the builder knows which
typed columns were created.

Use `sqlparser::parser::Parser` to parse the AST, walk and rewrite Expr nodes,
then re-serialize to SQL string. This is ~500-800 lines.

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

### Priority 3: Benchmark the v2 Path

Add to e2e_bench.rs or create a new benchmark:

```
Modes to benchmark:
  scan-only:          scanner → RecordBatch → discard
  scan+passthrough:   scanner → DataFusion "SELECT * FROM logs" → discard
  scan+filter:        scanner → DataFusion "WHERE level_str != 'DEBUG'" → discard
  scan+filter+otlp:   scanner → DataFusion → OtlpSink (to /dev/null)
  scan+filter+json:   scanner → DataFusion → StdoutSink (to /dev/null)
  full-pipeline:      file read → CRI → scanner → DataFusion → OtlpSink

Test data: /tmp/vm_format_cri.txt (5M lines, 257 bytes/line, 10 variable fields)
```

Performance targets:
- scan-only: ≤400ns/line (2.5M lines/sec)
- scan+passthrough: ≤500ns/line (2.0M lines/sec)
- scan+filter+otlp: ≤700ns/line (1.4M lines/sec)
- full-pipeline (no compression): ≤800ns/line (1.25M lines/sec)

### Priority 4: Input Abstraction

Create `/Users/bill.easton/repos/memagent/src/input.rs`:

```rust
pub trait InputSource: Send {
    fn poll(&mut self) -> io::Result<Option<InputBatch>>;
    fn name(&self) -> &str;
    fn stats(&self) -> &ComponentStats;
}

pub enum InputBatch {
    /// Raw bytes needing scanning (from file, UDP, TCP).
    Raw { buf: Vec<u8>, format: Format },
    /// Pre-structured (from OTLP input). Bypass scanner.
    Structured { batch: RecordBatch },
}
```

Implementations:
- `FileInput` — wraps tail.rs + cri.rs + chunk accumulator
- `UdpInput` — recvmmsg, syslog parsing
- `TcpInput` — newline-framed stream

### Priority 5: End-to-End Acks

Update the pipeline to track:
```rust
struct CursorState {
    committed_offset: u64,  // acked by all outputs
    read_offset: u64,       // current read position
    pending_batches: VecDeque<PendingBatch>,
    max_pending: usize,     // configurable: how far ahead of committed
}

struct PendingBatch {
    file_offset_start: u64,
    file_offset_end: u64,
    batch_id: u64,
}
```

Reader pauses when `pending_batches.len() >= max_pending`.
Output acks a batch_id → committed_offset advances.
On crash: restart from committed_offset (re-send pending batches = duplicates, not loss).

### Priority 6: New CLI / Main

Update main.rs to support:
```
logfwd --config /etc/logfwd/config.yaml     # run from YAML config
logfwd --config config.yaml --validate      # validate config, exit
logfwd --config config.yaml --dry-run       # parse first batch, show schema, exit
```

Keep the existing CLI modes (--generate, --e2e, --tail, --daemon) for backwards
compatibility and development benchmarking.

## Architecture Summary

```
YAML Config
    │
    ▼
┌─── Pipeline ──────────────────────────────────────────┐
│                                                        │
│  InputSource(s)                                        │
│    │                                                   │
│    ▼                                                   │
│  CRI Parser (if format == cri)                         │
│    │                                                   │
│    ▼                                                   │
│  Scanner (scanner.rs + batch_builder.rs)               │
│    │  JSON bytes → Arrow RecordBatch                   │
│    │  Typed columns: field_str, field_int, field_float │
│    │  _raw column: original line bytes                 │
│    ▼                                                   │
│  SQL Rewriter (rewriter.rs) [NOT YET BUILT]            │
│    │  user SQL → internal typed SQL                    │
│    ▼                                                   │
│  DataFusion (transform.rs)                             │
│    │  Execute SQL on RecordBatch                       │
│    │  Cached plan, schema evolution                    │
│    ▼                                                   │
│  OutputSink(s) (output.rs)                             │
│    │  Serialize RecordBatch → OTLP/JSON/ES/Parquet     │
│    │  FanOut to multiple sinks                         │
│    ▼                                                   │
│  Network / File / Stdout                               │
│                                                        │
│  Diagnostics Server (diagnostics.rs)                   │
│    GET /health, /metrics, /api/pipelines, /            │
│    Reads atomic counters from all components           │
└────────────────────────────────────────────────────────┘
```

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

### v1 Pipeline (current daemon path)
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

## Files You'll Likely Need to Read

- `docs/ARCHITECTURE_V2.md` — full v2 architecture with YAML schema and diagrams
- `docs/DATA_FUSION_PLAN_UPDATE.md` — detailed scanner/builder/transform design
- `docs/research/logfwd-research-findings.md` — benchmark data from DataFusion prototypes
- `docs/research/logfwd-plan.md` — phased build plan
- `DEVELOPING.md` — developer guide for the existing codebase
- `README.md` — user-facing documentation

## Testing

```bash
# Run all tests
cargo test

# Run specific module tests
cargo test scanner
cargo test transform
cargo test output
cargo test config
cargo test diagnostics

# Run v1 benchmarks (still work)
./target/release/logfwd --generate-json 5000000 /tmp/json.txt
./target/release/logfwd /tmp/json.txt --mode otlp

# Run v1 e2e benchmark with CRI parsing
./target/release/logfwd --e2e --wrap-cri /tmp/json.txt /tmp/cri.txt
./target/release/logfwd --e2e /tmp/cri.txt otlp-zstd

# Generate VM-format test data (matches VictoriaMetrics benchmark)
python3 -c "... (see e2e section in main.rs or use the script in the conversation)"
```

## K8s Benchmark Environment

The VictoriaMetrics benchmark infrastructure may still be running in KIND:
```bash
kind get clusters  # check for "log-collectors-bench"
kubectl get pods -A  # check cluster state
```

The benchmark repo is at `/tmp/vm-bench`. Our DaemonSet manifest is at `deploy/daemonset.yml`.
Both logfwd and vlagent were last tested at 250m CPU limits with the generator's
panic removed (see `/tmp/vm-bench/log-generator/main.go` — we changed `panic` to `continue`).

## Dependencies

```toml
arrow = "54"
datafusion = "45"
tokio = { version = "1", features = ["rt"] }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
smallvec = { version = "1", features = ["union"] }
tiny_http = "0.12"
memchr = "2"
zstd = "0.13"
xxhash-rust = { version = "0.8", features = ["xxh32", "xxh64"] }
notify = "7"
crossbeam-channel = "0.5"
ureq = "3"
```
