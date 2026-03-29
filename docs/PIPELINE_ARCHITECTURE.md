# logfwd Pipeline Component Architecture Specification

## Context

logfwd's pipeline grew organically — file input, SIMD scanner, SQL transform, OTLP/HTTP output — all wired together in a single-threaded loop. It works, but the boundaries between components are informal. As we add network inputs (#54), more outputs (#53), a disk queue (#72), backpressure (#45), retry (#41), and checkpointing (#44), we need formalized interfaces so components can evolve independently.

This document specifies each component's trait, data contract, error semantics, backpressure model, lifecycle, and metrics. The goal is a document we can hand to reviewers for validation before implementing.

## Key Design Decisions

**Source produces bytes OR structured data.** File/TCP/UDP sources produce raw bytes that need parsing + scanning. OTLP receiver input produces structured data that bypasses the scanner. A `SourceOutput` enum handles both.

**Disk queue sits after scanner, before transform.** Scanner is fast (~21 GiB/s). Transform is slow (creates tokio runtime + DataFusion context). Queuing after scan persists compact Arrow IPC data. The queue also buffers pre-output for retry. Two named segments of the same `DiskQueue` implementation.

**Sink is async.** HTTP, gRPC, and retry timers are inherently async. The pipeline runs on a tokio runtime. Sync sinks (stdout, file) use `spawn_blocking`.

**Backpressure via bounded channels.** `tokio::sync::mpsc` channels between stages. Sink slow → pre-output queue fills → transform blocks → pre-transform queue fills → scanner stops → source stops polling. Explicit, bounded, debuggable.

**Per-input identity preserved end-to-end.** Every batch carries a `SourceId`. Enables per-source checkpointing, enrichment, routing, and ack.

**Arrow RecordBatch is the universal interchange format** between scanner, queue, transform, and sink.

---

## Component 1: Source

Replaces current `InputSource` + `FormatParser`. Owns format parsing internally.

```rust
pub struct SourceId { pub id: u64, pub name: Arc<str> }

pub enum SourceOutput {
    Raw { json_lines: Vec<u8>, line_count: usize },
    Structured { batch: RecordBatch, metadata: BatchMetadata },
    Event(SourceEvent),  // Rotated, Truncated, Eof
}

#[async_trait]
pub trait Source: Send {
    async fn poll(&mut self) -> io::Result<Option<SourceOutput>>;
    fn ack(&mut self, checkpoint: &SourceCheckpoint);
    fn id(&self) -> &SourceId;
    fn checkpoint(&self) -> SourceCheckpoint;
    async fn shutdown(&mut self);
}

pub struct SourceCheckpoint {
    pub source_id: u64,
    pub position: Vec<u8>,  // opaque (file: device+inode+offset; network: empty)
    pub timestamp_ns: u64,
}
```

**Data contract:** `Raw.json_lines` is complete newline-delimited JSON, no partial lines. `Structured.batch` has scanner-compatible schema. `line_count` equals `\n` count.

**Errors:** Transient (file gone, network blip) → retry internally, return `Ok(None)`. Permanent (bad config) → return `Err`. Panic → pipeline restarts source from checkpoint.

**Backpressure:** Pipeline stops calling `poll()`. File: kernel buffers on disk. Network: TCP backpressure / UDP drops.

**Metrics:** `source_bytes_total`, `source_lines_total`, `source_errors_total`, `source_lag_bytes` (file only).

---

## Component 2: Scanner

Existing `SimdScanner`, formalized contract. Stays as concrete struct (no vtable on hot path).

```rust
pub trait Scanner: Send {
    fn scan(&mut self, buf: &[u8]) -> RecordBatch;
    fn update_config(&mut self, config: ScanConfig);
}
```

**Data contract:** Input is complete NDJSON lines. Output is `RecordBatch` with type-suffixed columns (`foo_str`, `foo_int`, `foo_float`), optional `_raw`. Rows = non-empty input lines. All columns same length, null-padded.

**Errors:** None returned. Malformed lines produce rows with only `_raw`. Scanner never drops data.

**Backpressure:** None intrinsic. Pipeline controls call rate.

**Metrics:** Recorded by pipeline: `scanner_rows_total`, `scanner_batch_duration_ns`, `scanner_bytes_total`.

---

## Component 3: DiskQueue

New component. Arrow IPC on disk. Crash recovery + retry buffer.

```rust
pub struct QueueEntry {
    pub segment: SegmentId,
    pub batch: RecordBatch,
    pub metadata: BatchMetadata,
    pub source_id: u64,
    pub source_checkpoint: SourceCheckpoint,
}

#[async_trait]
pub trait DiskQueue: Send {
    async fn push(&mut self, entry: QueueEntry) -> io::Result<SegmentId>;
    async fn peek(&mut self) -> io::Result<Option<QueueEntry>>;
    async fn ack(&mut self, segment: SegmentId) -> io::Result<()>;
    fn pending_count(&self) -> usize;
    fn pending_bytes(&self) -> u64;
    async fn replay(&mut self) -> io::Result<Vec<QueueEntry>>;
    async fn shutdown(&mut self) -> io::Result<()>;
}
```

**Data contract:** Arrow IPC files, one per segment. `manifest.json` tracks read/ack positions. `push()` blocks at size limit (backpressure). `ack(s)` frees entries ≤ s. `replay()` returns all unacked in order.

**On-disk:** `{queue_name}_{segment:016x}.arrow` + `manifest.json`. Default size limit: 1 GiB.

**Errors:** Disk full → `push()` errors, pipeline retries. Corrupt segment → skip + log. Missing dir → auto-create.

**Metrics:** `queue_entries_total`, `queue_pending_entries`, `queue_pending_bytes`, `queue_replay_entries`.

---

## Component 4: Transform

Existing `SqlTransform`, made async. Reuses DataFusion context across batches.

```rust
pub enum TransformError { Execution(String), Internal(String) }

#[async_trait]
pub trait Transform: Send {
    async fn execute(&mut self, batch: RecordBatch) -> Result<RecordBatch, TransformError>;
    fn scan_config(&self) -> ScanConfig;
    fn name(&self) -> &str;
}
```

**Data contract:** Input has scanner schema. Output has SQL-determined schema (columns may be renamed/added/removed). Empty input → empty output immediately.

**Errors:** `Execution` → drop batch, continue. `Internal` → retry once, then drop. Must not panic.

**Key change from current code:** Stop creating new tokio runtime per batch. Share pipeline's runtime. Cache DataFusion `SessionContext`, only re-register MemTable per call.

**Metrics:** `transform_rows_in_total`, `transform_rows_out_total`, `transform_errors_total`, `transform_batch_duration_ns`, `transform_filter_ratio`.

---

## Component 5: Sink

Replaces current sync `OutputSink`. Async with explicit retry semantics.

```rust
pub enum SendResult {
    Ok,
    RetryAfter(Duration),
    Rejected(String),
}

#[async_trait]
pub trait Sink: Send {
    async fn send_batch(&mut self, batch: &RecordBatch, metadata: &BatchMetadata) -> io::Result<SendResult>;
    async fn flush(&mut self) -> io::Result<()>;
    fn name(&self) -> &str;
    async fn shutdown(&mut self) -> io::Result<()>;
}
```

**Data contract:** Input is post-transform RecordBatch + BatchMetadata (with SourceId, batch_seq). `send_batch` is idempotent for same `(source_id, batch_seq)`.

**Retry:** `RetryAfter(d)` → exponential backoff (base=1s, max=60s, jitter=500ms). `Rejected` → drop batch + WARN log. `io::Error` → treat as transient, retry.

**Backpressure:** `RetryAfter` signals slowness. FanOut handles sinks independently — slow sink doesn't block fast sinks. Pre-output queue absorbs delay.

**Metrics:** `sink_batches_total`, `sink_rows_total`, `sink_bytes_total`, `sink_errors_total`, `sink_rejected_total`, `sink_retry_total`, `sink_send_duration_ns`.

---

## Component 6: Pipeline (Orchestrator)

Wires all components with bounded async channels.

```
Task 1: Source Loop (per source)     → tx_pre_transform
Task 2: Queue Writer                  rx_pre_transform → pre_transform_queue
Task 3: Transform Loop               pre_transform_queue → pre_output_queue
Task 4: Sink Loop (per sink/fanout)  pre_output_queue → sinks → ack → checkpoint
Task 5: Checkpoint Flusher           periodic flush + source ack
```

**Shutdown:** `CancellationToken` replaces `AtomicBool`. Tasks drain in reverse order: sources stop → scanner finishes → queues flush → transform completes → sinks flush → checkpoints persist.

**Config:**
```rust
pub struct PipelineRuntimeConfig {
    pub channel_capacity: usize,        // 16
    pub batch_target_bytes: usize,      // 4 MiB
    pub batch_timeout: Duration,        // 100ms
    pub checkpoint_interval: Duration,  // 5s
    pub queue_max_bytes: u64,           // 1 GiB
    pub retry_base: Duration,           // 1s
    pub retry_max: Duration,            // 60s
}
```

---

## Component 7: Checkpoint

Tracks per-source progress for crash recovery.

```rust
#[async_trait]
pub trait CheckpointStore: Send {
    fn update(&mut self, checkpoint: SourceCheckpoint);  // in-memory
    async fn flush(&mut self) -> io::Result<()>;         // persist to disk
    fn load(&self, source_id: u64) -> Option<SourceCheckpoint>;
    fn load_all(&self) -> Vec<SourceCheckpoint>;
}
```

**On-disk:** `{data_dir}/checkpoints.json`, atomic write via rename. Updated every 5s + on shutdown.

**Guarantee:** After `flush()` returns Ok, checkpoint survives crash + power loss.

---

## Column Naming Convention (Cross-Cutting)

```rust
pub mod column_naming {
    pub const SUFFIX_STR: &str = "_str";
    pub const SUFFIX_INT: &str = "_int";
    pub const SUFFIX_FLOAT: &str = "_float";
    pub const RAW_COLUMN: &str = "_raw";
    pub const META_PREFIX: &str = "_meta_";

    pub fn parse(col: &str) -> (&str, &str);     // "level_str" → ("level", "str")
    pub fn strip_suffix(col: &str) -> &str;       // "level_str" → "level"
}
```

**Rules:**
1. Scanner ALWAYS produces type-suffixed columns
2. `_raw` is the sole exception (no suffix)
3. Metadata columns use `_meta_` prefix + suffix
4. Transform output columns don't require suffixes
5. Sinks strip suffixes for serialization (`"level": "INFO"` not `"level_str": "INFO"`)
6. Multi-type columns: sinks prefer int > float > str

---

## BatchMetadata (Extended)

```rust
pub struct BatchMetadata {
    pub resource_attrs: Vec<(String, String)>,
    pub observed_time_ns: u64,
    pub source_id: SourceId,
    pub batch_seq: u64,  // monotonic per pipeline, for dedup
}
```

---

## Migration Path

**Phase 1: Formalize (no behavior change)**
- Extract `Source` trait from `InputSource` + `FormatParser`
- Make `OutputSink` async → becomes `Sink`
- Add `SourceId` and `batch_seq` to `BatchMetadata`

**Phase 2: DiskQueue + Checkpoint**
- Implement `FileCheckpointStore` (JSON + atomic rename)
- Implement `ArrowDiskQueue` (Arrow IPC files + manifest)
- Wire into pipeline between scanner and transform

**Phase 3: Async Pipeline**
- Replace sync `Pipeline::run()` with tokio task graph
- Replace `AtomicBool` shutdown with `CancellationToken`
- Add bounded mpsc channels between stages

**Phase 4: Fix Transform**
- Share pipeline's tokio runtime (stop creating per-batch)
- Cache DataFusion SessionContext
- Make `execute()` async

## Related Issues

Bugs: #74, #75. Production: #40-48. Performance: #23-27, #29, #31. Features: #28, #30, #34-37, #39, #53-54, #69. Storage: #70-72. Infra: #34, #49-52, #67.

## Verification

This is a design document, not a code change. Validation:
1. Share with review agents for questions/concerns
2. Validate against each open issue — does the architecture support it?
3. Prototype the DiskQueue (smallest new component, validates Arrow IPC round-trip)
4. Prototype async Sink (validates the sync→async migration path)
