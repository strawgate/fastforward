# Design

ffwd is an Arrow-native data processing pipeline. The core moves `RecordBatch` between
receivers, processors, and exporters. OTel semantics are optional adapters at the edges.

**RecordBatch in, RecordBatch out.** The pipeline doesn't care whether the data is logs,
metrics, traces, or CSV files.

## Target architecture

```
ffwd-core         Proven pure logic. #![no_std], #![forbid(unsafe_code)]
                    Parsing, encoding, pipeline state machine.
                    Streaming structural classification, framer, CRI, scanner.
                    Dependencies: memchr, wide (portable SIMD).
                    Every public function has a Kani proof or proptest.

ffwd-arrow        Arrow integration. Implements core's ScanBuilder trait.
                    StreamingBuilder, Scanner.
                    Bridge between parsed fields and RecordBatch.

ffwd-io           Produces RecordBatch from external sources.
                    File tailer, OTLP/TCP/UDP receivers, Arrow IPC reader.
                    Raw data goes through core→arrow→RecordBatch.
                    Arrow-native sources skip straight to RecordBatch.

ffwd-transform    RecordBatch → RecordBatch via DataFusion SQL.
                    UDFs, enrichment tables, JOINs.
                    CSV enrichment tables produce Utf8View columns via the
                    shared columnar builder.

ffwd-output       Consumes RecordBatch, sends externally.
                    OTLP, Arrow IPC, JSON lines. Parquet/ClickHouse are planned.

ffwd-runtime      Async runtime shell. Pipeline orchestration,
                    worker pool, processor chain, diagnostics wiring.

ffwd              Binary shell. CLI, config loading, signal handling,
                    startup/shutdown bootstrap, compatibility re-exports.
```

## Ecosystem interop

| System | Receive | Export |
|--------|:-------:|:------:|
| OTLP protobuf | ✓ | ✓ |
| Arrow IPC files | ✓ | ✓ |
| Parquet files | — | planned |
| ClickHouse (HTTP ArrowStream) | — | planned |
| Arrow Flight (gRPC) | planned | planned |
| OTel Arrow (OTAP) | planned | planned |

OTAP: stay independent of the official otap-dataflow crates (unpublished, heavy deps,
unstable protocol). When we implement OTAP, use the proto file directly and implement
star-to-flat / flat-to-star conversion at the boundary.

## Performance target

1M+ events/sec end-to-end. Key bottleneck is output transport throughput — the async pipeline overlaps IO
with CPU. SIMD structural scanning runs at 3.0 GiB/s (9 structural characters, NEON,
~760KB NDJSON, 2026-03-30 benchmark).

## What we're not building

- A general-purpose OTel Collector
- A database or query engine (we use DataFusion)
- A distributed system (scale by running more instances)

---

## Architecture decisions

### no_std structural enforcement for ffwd-core

`#![no_std]` with `extern crate alloc`. The compiler blocks IO — not lints that can be
`#[allow]`'d. This is the rustls/s2n-quic pattern. Four independent research studies
unanimously recommended this over std + clippy.

**Cost:** `core::`/`alloc::` import paths, hashbrown for HashMap if needed, memchr with
`default-features = false`.  
**Benefit:** Impossible to accidentally add IO to the proven core.

### ScanBuilder trait boundary (zero-allocation parsing)

Generic trait (serde Visitor pattern), not `Vec<ParsedField>`. Data flows directly from
parser into builder via monomorphization — no intermediate allocation. This is how serde
achieves zero-cost serialization.

**Alternative considered:** `SmallVec<[ParsedField; 16]>` return type. Vec is often
faster than SmallVec in benchmarks, and the callback pattern eliminates allocation entirely.

### Two-stage architecture: SIMD detects, scalar consumes

Stage 1 (SIMD character detection) and Stage 2 (parsing/field extraction) are separate.
SIMD produces bitmasks; a sequential scanner consumes them.

SIMD is embarrassingly parallel within a 64-byte block. Parsing is inherently sequential —
the meaning of a comma depends on whether we're inside a string. These are fundamentally
different parallelism profiles.

Stage 1 runs one SIMD pass over the entire buffer (via `wide` crate), detecting structural
characters and producing `u64` bitmasks. Cross-block state: only 2 `u64` values (escape
carry, string interior carry). Stage 2 walks structural positions using `trailing_zeros` +
clear-lowest-bit. This is the proven simdjson architecture.

### Scalar SIMD fallback in ffwd-core

`#![forbid(unsafe_code)]` is in core. Portable SIMD is provided by the `wide` crate,
which exposes a safe API for vectorized operations. Core has a safe scalar
`find_structural_chars_scalar` that is the Kani-provable specification.
`find_structural_chars` uses `wide` to provide portable SIMD across NEON,
AVX2, and SSE2 within the safe-code boundaries of core. proptest verifies
SIMD matches scalar for random inputs.

### Pipeline state machine over linear BatchToken

Prove pipeline correctness via a pure state machine in core (Kani single-step + TLA+
liveness), not a linear type.

Strictly linear `BatchToken` is impossible with async cancellation: Tokio can cancel a
future at any `.await` point, so a `#[must_use]` token can be dropped without being
consumed. The type system cannot prevent this.

The state machine in core is proven correct: no state exists where a batch is "forgotten."
The async shell handles best-effort delivery (at-least-once) using ordered ACK via BatchId.

**Related:** #270, #272.

### Opaque checkpoints (`C` type parameter)

`BatchTicket<S, C>` and `PipelineMachine<S, C>` are generic over checkpoint type `C`. The
pipeline stores and forwards checkpoints without interpreting them.

"Offset" means different things per input: file tail is `u64` byte position, Kafka is `i64`
partition offset (gaps from compaction), Journald is an opaque cursor string, push sources
(OTLP, syslog) have no checkpoint at all. A `u64` with contiguity enforcement would only
work for file inputs.

**Cost:** `C: Clone` bound on methods; generic propagates to `AckReceipt<C>`,
`CheckpointAdvance<C>`.  
**Research:** `dev-docs/research/checkpoint-snapshot-design.md`. **Related:** #270.

### Kani checkpoint proof domain

The `pipeline/lifecycle.rs` Kani harnesses may narrow symbolic checkpoint values
to a small integer domain when the proof only checks lifecycle behavior. This is
valid because `PipelineMachine<S, C>` stores, orders, and returns checkpoints
without interpreting their arithmetic meaning. TLA+ coverage is unchanged: the
`PipelineMachine.tla` model already treats checkpoint values as opaque tokens
while checking batch sequencing, ACK/reject advance, drain, and ordering
invariants.

`verify_committed_monotonic` also assumes the batches of one source carry
nondecreasing checkpoints in creation order. The machine commits each batch's
own checkpoint, so the committed checkpoint is monotonic only when that
precondition holds. The TLA+ model has it built in: there `committed[s]` is a
batch sequence number. Real inputs do not always satisfy it. When a tailed file
is truncated, the reader resets its offset to 0 (`ffwd-io` `tail/lifecycle.rs`),
and if the rewritten file keeps the same device, inode and leading bytes it
keeps its `SourceId`, so later batches carry smaller offsets and the committed
checkpoint goes back with them. The proof does not cover that path.

### `BTreeMap` replaced by `ArrayMap` under Kani

**Context:** `PipelineMachine` and `SourceRegistry` keep their state in
`BTreeMap`s. CBMC cannot bound a B-tree's height, so every map operation
unrolled node walks and drop glue to the full unwind bound. Even one-batch
lifecycle proofs ran out of memory, and 18 of the 20 lifecycle and registry
harnesses had to leave PR CI.

**Decision:** both use `OrdMap` (`pipeline/ord_map.rs`). It is
`alloc::collections::BTreeMap` in every normal build and `ArrayMap<K, V, 4>`
under `cfg(kani)`: a sorted array of `Option<(K, V)>` slots with the subset of
the `BTreeMap` API the pipeline uses.

**Consequences:** the Kani proofs verify the state-machine logic against
`ArrayMap`, not `BTreeMap`. A proptest compares every `ArrayMap` operation with
`BTreeMap`, which covers that gap. Proofs may hold at most 4 entries per map;
going past that panics, so a proof that outgrows it fails instead of passing.
Production code and its behavior are unchanged.

**Alternatives considered:** a sorted `Vec` fixed single-batch proofs but not
the ack path, where nested heap buffers move through `memmove`. Shrinking the
harnesses alone was not enough: a two-batch proof still ran out of memory with
`BTreeMap`. Keeping the proofs nightly-only meant a real counterexample would
look the same as a resource failure.

### Rejected batches advance the checkpoint

`RejectBatch` has the same state transition as `AckBatch` — permanently-undeliverable data
must not block checkpoint progress indefinitely, which would stall drain forever. At-least-
once delivery is weakened to at-most-once only for rejected batches.

This matches Filebeat's behavior (advance past malformed records) and differs from Fluent
Bit (drops the route, retries via a backlog). If a batch is rejected, the data in that
batch is lost — the correct behavior for a forwarder where corrupted or oversized data
cannot be retried, but it must be metered.

### Output retries are bounded once shutdown begins

Output workers retry transient failures forever while running, so a down output turns into
backpressure, not data loss. Once shutdown begins (`ShutdownRetryGrace::begin`, called as
soon as the pipeline's shutdown token is cancelled), workers keep retrying with their normal
backoff for a bounded window: `shutdown_retry_grace_ms`, default 10s, capped by the pool
drain timeout. When the window closes, a worker in backoff gives up at once and later
transient failures are not retried: the batch resolves as `PoolClosed` (held, checkpoint not
advanced). The window bounds retries cooperatively: a retry can still start at or just after
expiry, and a retry that is mid-send is abandoned at its next poll, with the batch held
(at-least-once). Because an abandoned send can leave the sink mid-request, that worker then
holds its remaining batches without sending them. A batch's first attempt still runs until it completes or the drain timeout
cancels the worker. Without a bound, a down
output held every shutdown open for the full pool drain timeout (60s), which is longer than the Kubernetes default termination grace period (30s).
A 10s default leaves room inside that period while still riding out a short collector
restart. `0` gives up on the first failure after shutdown begins.

Trade-off: checkpointed inputs (files) replay held batches on restart. Non-checkpointed
inputs (TCP, UDP, OTLP receivers) lose a batch that is still failing when the window closes.
They already lost it after the drain timeout, and usually to SIGKILL first. This sits between
Filebeat's default (`shutdown_timeout: 0`, no waiting for acks at shutdown) and Vector, which
waits up to its graceful shutdown limit (60s). Modeled in `tla/DeliveryRetry.tla`
(`BeginShutdown`/`StopRetrying`, checked under `GracefulSpec`).

### Shutdown drains queued batches within one drain budget

On SIGTERM or a config reload, the bootstrap loop cancels the pipeline token and keeps
awaiting the same `run_async` future (`drive_main_pipeline`). Dropping that future and
calling `run_async` again discarded every batch queued in the input channel, because the
first call had already taken the inputs and owned the channel. `run_async`'s own shutdown
path now runs once: submit queued batches, close the channel receiver, join input threads,
drain the pool, flush checkpoints.

Submitting queued batches and draining the pool share one budget, `pool_drain_timeout`
(60s), counted from when shutdown was requested. Once it is spent, batches still queued are
left unsent and the pool drains with whatever is left. Checkpoints for unsent batches never
advance, so file inputs replay them; inputs without checkpoints lose them. Without the
budget, a collector that accepts connections but stalls each send would make every queued
batch wait out a send timeout in turn. A `submit_batch` already in progress is not cancelled,
because dropping its `Sending` tickets would block the drain, so the worst case is the budget
plus one batch send. The budget bounds graceful delivery work, not total exit time: pool
cancellation grace and checkpoint flushing can follow it. A config reload additionally caps
its wait for the old pipeline at 30s (`RELOAD_DRAIN_TIMEOUT`), so queued batches get at most
that long before the new pipeline starts. The receiver is closed before joining producers on every path, so a
drain that stops early cannot leave a producer parked on the full channel.

### Per-source independent checkpoints

Each source has its own `committed[s]`. A slow source doesn't block a fast source from
committing. Equivalent to per-partition independent offsets in Kafka. Differs from Flink's
global checkpoint barrier, which is required for stateful stream processing but adds
blocking a stateless forwarder should not need.

### `fail()` is invisible to the pipeline machine

A `fail()`'d batch returns its ticket to Queued state, but the machine still tracks it as
`in_flight` — it was already `begin_send`'d. `fail()` changes no machine state. The
BTreeMap entry in `in_flight[source]` is not removed until `apply_ack` is called.

This models the Rust code exactly: `fail()` returns `BatchTicket<Queued, C>` but does not
touch the BTreeMap.

Current implication at the worker/checkpoint seam: a non-advancing failure
cannot be resolved by calling `fail()` alone unless the runtime still holds the
batch payload and can resubmit it. Until that richer retry path exists, held
worker outcomes remain unresolved and are replayed after restart if shutdown
force-stops with tickets still in flight.

### Suffix only on type conflict

Bare column names by default. Suffixed columns (`_int`, `_str`, `_float`) only when a
field has multiple types within a batch.

**Core requirement — round-trip type fidelity.** A field that enters as `int 200` must
exit as `int 200`. No type promotion across documents.  
**Core requirement — clean schemas stay clean.** OTLP, Arrow IPC, CSV, and consistent JSON
produce bare column names with native types. CSV enrichment columns are bare nullable
`Utf8View` string columns. The suffix machinery only activates when there is an actual
per-row type conflict.

Output always serializes with the bare JSON key, dispatching on DataType. A view layer
provides bare-name access for suffixed columns.

**Research:** `dev-docs/research/type-suffix-redesign.md`. **Related:** #445.

### Strict string-vs-number SQL comparisons (follow DataFusion)

`WHERE status >= 400` on a Utf8 column uses DataFusion's coercion: the column is cast
to the numeric type with a strict `CAST`, so numeric strings compare numerically and a
non-numeric value fails the transform (the batch is dropped).

**Why:** match upstream semantics. Leniency can be added later as an opt-in without
breaking anyone; making it the default would turn today's failed batches into output,
which callers could observe.
Users opt in per expression with `int()` / `float()`, which return NULL on bad input.  
**Considered:** an analyzer rule that rewrites the implicit cast to `TRY_CAST` for
comparisons, `BETWEEN` and `IN` (prototyped in PR #2921). Deferred; revisit if
dropped batches from mixed-type fields show up in practice.  
**Rejected:** restoring DataFusion 53's string comparison (`'50' >= 400` is true).

### Plan transform SQL once, reuse the logical plan

`SqlTransform` parses and plans its SQL on the first batch and keeps the unoptimized
`LogicalPlan`. Each later batch builds a `DataFrame` from that plan; the optimizer and
physical planning still run per batch, so volatile functions such as `now()` are
evaluated fresh. The plan holds its table providers, so `logs` and enrichment tables
are registered as `SwapTable`s (fixed schema, swappable `MemTable`) whose data is
replaced in place. A new input schema, or an enrichment table appearing, disappearing
or changing schema, drops the cached plan and replans.

**Why:** DataFusion 55's parse + analyze step is about 2x slower than 53's and ran on
every batch. Caching it makes steady-state transforms 10–48% faster than before the
upgrade.  
**Rejected:** caching the optimized or physical plan (would freeze `now()` and other
volatile expressions, and the physical plan binds the batch data).

### Flat schema (not OTAP star schema)

Single `RecordBatch` with all fields as columns. `resource.attributes.*` prefix
for resource attributes. Directly queryable by DuckDB, Polars, DataFusion with
zero schema knowledge.
OTAP's star schema (4+ tables with foreign keys) is optimized for wire efficiency, not
queryability. Convert at the boundary when needed.

### bytes::Bytes for pipeline buffer ownership

The ingest path uses `BytesMut` while a component still owns mutable fill state,
then hands off immutable `Bytes` once ownership crosses a boundary. This is the
standard Rust pattern for I/O pipelines where buffers cross thread/async
boundaries and may need to outlive the original producer.

Why `Bytes` instead of `Vec<u8>`: The Arrow `StreamingBuilder` needs the input
buffer to outlive the scan phase — `StringViewArray` stores `(offset, len)` views
into the buffer, and the `RecordBatch` carries these through SQL transform and
output serialization. `Bytes` provides this lifetime extension via refcounting.
`Vec<u8>` cannot be shared without cloning.

Why not `Arc<Vec<u8>>`: `Bytes` supports zero-copy `slice()` and `split()` that
`Arc<Vec<u8>>` does not. That matters both for the zero-copy passthrough framing
path already on `main` and for the shared-buffer framing path now landing in
`FramedInput`, which still needs to preserve one contiguous scanner input.

Current boundary:

- `ffwd-io` tailing uses per-source `BytesMut` read buffers.
- `SourceEvent::Data` already carries `Bytes`.
- `FramedInput` may still keep small `Vec<u8>` remainders where a line is
  incomplete or overflow-tainted.
- `ffwd-runtime` currently performs the remaining pre-scan accumulation into
  `IngestState.buf: BytesMut`.
- `ffwd-core` is untouched by this ownership shift; the scanner still takes
  contiguous `Bytes` via `Bytes::Deref`.

Current implication: the remaining hot-path copy on `main` is not “tailer to
event” anymore. It is the runtime reassembly step that appends scanner-ready
`Bytes` into `IngestState.buf` before scan on the legacy event route. The next
architecture slice should widen the shared-buffer path so more polls append
directly into that final batch buffer before scan, targeting that seam
directly rather than reintroducing divergent source-side batching paths.

### Verification strategy

Three tools, each at a different layer. See `dev-docs/VERIFICATION.md` for mechanics.

| Layer | Tool | What it proves |
|-------|------|----------------|
| Design | TLA+ | Temporal properties: liveness, drain guarantee, checkpoint ordering |
| Implementation | Kani | Memory safety, arithmetic, no-panic, state transitions |
| Property-based | proptest | End-to-end correctness under arbitrary inputs |
| Compile-time | Rust type system | Illegal state transitions, silent data loss |

TLA+ proves the design is correct — no race conditions, correct ordering, drain eventually
completes. Kani proves the Rust implementation doesn't panic or overflow. A design bug is
caught by TLA+; an implementation bug is caught by Kani.

### Checkpoint as a pipeline step

Checkpoint is a first-class pipeline step that persists RecordBatches to disk between
transforms. Users control placement — the framework validates but doesn't dictate.

```yaml
pipelines:
  web_errors:
    inputs:
      - type: file
        path: /var/log/app/*.log
    steps:
      - type: transform
        sql: SELECT * FROM logs WHERE level IN ('ERROR', 'WARN')
      - type: checkpoint
        buffer_size: 256MB
        retention: 1h
      - type: transform
        sql: |
          SELECT level, message,
                 regexp_extract(message, 'request_id=([a-f0-9]+)', 1) AS req_id
          FROM logs
    outputs:
      - type: otlp
        endpoint: https://collector:4318
```

**Semantics:** The checkpoint step receives a RecordBatch, writes it to an Arrow IPC + zstd
segment on disk, and passes it through unchanged. On crash recovery, the pipeline replays
from the checkpoint's buffer instead of re-reading from the source.

**Config validation rules:**
- Checkpoint must come after an input source (not floating)
- Checkpoint must come before at least one output sink
- At most one checkpoint per input-to-output path
- Transforms downstream of checkpoint must be idempotent (no NOW(), RANDOM())

**Checkpoint coordination with input sources:**
- When the checkpoint buffer is durably flushed to disk (fsync), the input source's
  checkpoint (file offset or network sequence) advances atomically in the same
  `CheckpointStore::flush()` call
- On crash: replay from the input source checkpoint, which is guaranteed to be ≤ the
  checkpoint buffer's starting point
- File inputs: the file is the source-of-truth persistence; checkpoint step stores filtered output
- Network inputs: checkpoint step IS the only persistence

**Segment format:**
```
[4B magic: LCHK]
[4B version: 1]
[8B segment_id]
[8B sql_hash]          -- hash of upstream SQL, for schema change detection
[Arrow schema]         -- embedded for self-describing segments
... Arrow IPC stream batches (length-prefixed, zstd compressed) ...
[8B record_count]      -- footer
[4B CRC-32]
[4B magic: LCHK]       -- end marker
```

On recovery: read footer first. Missing/corrupt footer → discard segment. Schema hash
mismatch with current config → error with remediation ("clear buffer or revert SQL").

**Memory model:**
- RecordBatch wrapped in `Arc<RecordBatch>` — one ref for downstream, one for async writer
- Async writer runs in background tokio task with bounded channel (default 10K batches)
- When channel full → backpressure propagates upstream
- LRU eviction of in-memory segment cache when over memory budget

**Fan-out GC:**
- Per-output-sink ack watermark tracks which segments each sink has processed
- Segment GC when ALL sinks have acked (min watermark)
- Dead sink timeout: if no ack in 5 min, log error and force-advance that sink's watermark
- Prevents unbounded buffer growth from permanently dead sinks

**Alternative considered:** Framework-injected barriers (Flink model). Rejected because it
requires global coordination across sources, which conflicts with our per-source independent
checkpoint architecture. The user-placed checkpoint step gives control without barrier
complexity. If we add cross-source JOINs later, we can layer barriers on top.

### Pipeline Topology Compiler (#1363)

The pipeline topology compiler transforms a parsed configuration into a typed, verified execution DAG without starting any long-running workers.

**Phase 1: Dry-run scaffold**
- Introduce `PipelineSpec` and `CompiledTopology` models.
- Provide `compile_topology(config)` that builds the static DAG.
- Integrate into `ff dry-run` to execute this compiler, ensuring SQL plans and I/O shapes are correct statically.

**Future Phases (Not in Phase 1):**
- Translating the compiled topology into runtime Tokio tasks.
- Cross-pipeline routing.
- Advanced processor chain registry.

### Prometheus scrape: metric rows in Arrow, OTLP metrics as an output mode (#2924)

**Parser and batch shape.** `ffwd_core::prom_text` scans the text exposition
with the same SIMD-detect / scalar-consume split as the JSON scanner, behind a
`PromBuilder: ScanBuilder` boundary. `ffwd_arrow::prom::PromBatchBuilder` is a
dedicated builder rather than `StreamingBuilder`: every label value becomes a
`Utf8View`: long unescaped values point into the response buffer, short ones are
inlined, and only values with escapes are decoded into a separate buffer. Text to `RecordBatch` measured
770-970 MB/s, against 115-170 MB/s for Go's `textparse` plus labels. One row per sample, one column per label
name, plus `__name__`, `__value__` (Float64), `__timestamp_ms__`, `__type__` and an
order-independent `__series_id__` hash. Rows stay queryable by SQL like any other
input.

**Strict parsing.** A malformed line rejects the whole scrape, as Prometheus does,
so a partial scrape is never mistaken for a complete one. `max_label_columns`
(default 1000) and a padded-cell budget bound memory for hostile expositions.

**Compression.** `compression: none` is the default; gunzip costs about as much as
parse plus Arrow, and exporters are usually local. `gzip` is opt-in.

**OTLP metrics.** Exported through `signal: auto | logs | metrics` on the existing
`otlp` output rather than a processor or a new output type. `auto` sends batches
with a string `__name__` and numeric `__value__` to the metrics endpoint, so SQL
that produces those columns is log-to-metric for free. Histograms and summaries
are rebuilt from their component samples; incomplete or inconsistent ones
(missing count, decreasing or repeated buckets, a `+Inf` bucket that is not
exactly `_count`, quantiles outside `[0, 1]`) are dropped rather than guessed.
Counts are rounded to integers only when written. GaugeHistograms are dropped,
as the OpenTelemetry Prometheus compatibility spec requires. OpenMetrics `<family>_created` samples (start
times) keep their row with `__type__ = created` and are not exported as points
until start-time tracking exists.

**Alternative considered:** a separate metrics data model in the pipeline. Rejected
because it would fork every transform and sink; metric rows reuse the log
pipeline unchanged.
