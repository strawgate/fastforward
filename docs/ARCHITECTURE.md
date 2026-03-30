# Architecture

## Core principle

The pipeline is generic Arrow tables in, Arrow tables out. The core
knows nothing about OTel, logs, metrics, or any specific data model.
It moves RecordBatches between receivers, processors, and exporters.

OTel semantics (OTLP encoding, severity mapping, trace context,
`_resource_*` grouping) are optional features implemented as specific
receiver/exporter types. You could use this pipeline to process CSV
files into Parquet on S3 with no OTel involvement.

## Data flow

```
Receivers (produce RecordBatch)
    │  file scanner, Arrow IPC reader, ES|QL client,
    │  ClickHouse query, Arrow Flight, OTLP decoder, CSV/JSON/Parquet
    │
    │  each receiver produces RecordBatch independently
    ▼
RecordBatch per receiver
    │
    ├─→ [if queue.mode: pre-transform] write Arrow IPC segment → ack source
    │
    ├─→ register as partitions of MemTable
    │   (DataFusion concatenates partitions during query)
    ▼
SQL Processor (DataFusion)
    │  user SQL: SELECT, WHERE, GROUP BY + UDFs
    │  enrichment tables available via JOIN
    │  all columns flow through — no special treatment
    ▼
Post-transform RecordBatch
    │
    ├─→ [if queue.mode: post-transform] write Arrow IPC segment → ack source
    │
    ▼
Exporters (consume RecordBatch)
    │  Arrow IPC files, ClickHouse ArrowStream, Arrow Flight,
    │  Parquet, OTLP, JSON lines, stdout
    │  in-memory fan-out via bounded channels
```

## Receivers

Receivers produce Arrow RecordBatches from external sources. Each
receiver type handles its own protocol and serialization.

| Receiver | Protocol | Input format |
|----------|----------|-------------|
| `file` | Local filesystem | CRI / JSON / Raw → SIMD scanner → Arrow |
| `arrow_ipc` | Local / S3 / HTTP | Arrow IPC stream or file |
| `parquet` | Local / S3 | Parquet → Arrow (via DataFusion) |
| `clickhouse` | HTTP | `SELECT ... FORMAT ArrowStream` |
| `elasticsearch` | HTTP | ES\|QL `?format=arrow` |
| `arrow_flight` | gRPC | Flight `DoGet` / `DoPut` |
| `influxdb` | gRPC | Flight SQL query → Arrow |
| `otlp` | gRPC / HTTP | Protobuf → Arrow conversion |
| `csv` | Local / S3 / HTTP | CSV → Arrow (via `arrow-csv`) |
| `json` | Local / S3 / HTTP | JSON → Arrow (via `arrow-json`) |
| `kafka` | Kafka protocol | Arrow IPC or Avro message bytes |

Receivers can produce data lazily — handing off raw bytes that convert
to Arrow on demand (when a processor needs columnar access).

## Exporters

Exporters consume Arrow RecordBatches and write to external systems.

| Exporter | Protocol | Output format |
|----------|----------|--------------|
| `arrow_ipc` | Local / S3 | Arrow IPC stream (`.arrows`) or file (`.arrow`) |
| `parquet` | Local / S3 | Parquet (better compression for cold storage) |
| `clickhouse` | HTTP | `INSERT ... FORMAT ArrowStream` |
| `arrow_flight` | gRPC | Flight `DoPut` / `DoExchange` |
| `otlp` | gRPC / HTTP | Arrow → Protobuf, group by `_resource_*` |
| `json_lines` | HTTP / file | Arrow → NDJSON |
| `stdout` | stdio | Human-readable table |
| `kafka` | Kafka protocol | Arrow IPC as message bytes |

## Processors

Processors transform RecordBatches. The primary processor is DataFusion
SQL, but the interface is just `RecordBatch → RecordBatch`.

| Processor | Description |
|-----------|-------------|
| `sql` | DataFusion SQL: SELECT, WHERE, JOIN, GROUP BY, UDFs |
| `filter` | Row-level filtering (shorthand for `WHERE` clause) |
| `route` | Content-based routing to different exporters |
| `batch` | Combine small batches into larger ones |
| `fanout` | Duplicate to multiple downstream processors/exporters |

## Persistence

Arrow IPC is the universal segment format. Every persistence point
writes Arrow IPC File format with atomic seal, and every persistence
point can target local disk or object storage (S3/GCS/Azure Blob).

**Queue mode is configurable per pipeline:**

```yaml
pipeline:
  queue:
    mode: pre-transform   # or post-transform, or none
    storage: s3://my-bucket/logfwd/pipeline-a/
    max_bytes: 10GB
    max_age: 24h
```

**`pre-transform`:** Source segments are the queue. Each source writes
its own Arrow IPC segments after scanning. Outputs replay from source
segments and re-run the transform. Raw data is re-queryable with
different SQL. One copy of data. Enrichment data may be stale on replay
(`_resource_*` columns are always correct; enrichment table data
reflects query-time state, not ingest-time).

**`post-transform`:** Shared post-transform queue per pipeline.
Transform runs once, result is persisted. All outputs share one queue
with independent cursors. Enrichment baked in at ingest time — correct
even on replay days later. Can't re-query with different SQL. Second
copy of data (but usually smaller due to filtering).

**`none` (default):** Pure in-memory fan-out. Transform runs once,
RecordBatches passed to outputs via bounded channels. Batches drop on
output failure. Lowest resource usage.

## End-to-end ack

Sources must NOT acknowledge to their upstream until persistence
confirms durability. The ack chain:

```
Scanner produces RecordBatch
  → FileWriter writes to .tmp
  → fsync + rename (segment sealed)
  → Ack source: "data through offset X is durable"
  → Source advances checkpoint
```

Ack latency is scanner + IPC write + fsync, typically 5-15ms. Comparable
to Kafka's acks=all (3-15ms).

**File sources** don't need the ack latency to be fast — the log file is
already durable. The checkpoint just tracks the file offset.

**Network sources** see the full ack latency as their response time.

**When `queue.mode: none`:** No persistence, no ack-after-write. Source
checkpoints advance after in-memory fan-out. Data can be lost on crash.

## Output delivery

**Real-time path (all modes):** Transform runs once per batch cycle,
result fans out to all outputs via bounded channels.

**Replay (when queue is enabled):** If an output falls behind or
restarts, it replays from the queue. With `pre-transform` mode, replay
re-runs the transform. With `post-transform` mode, replay reads
transformed segments directly.

**Cursor tracking:** Each output tracks `{ segment_id, batch_offset }`.
Persisted on each ack. On restart, resume from last position.

**Retention:** TTL (`max_segment_age`) + size (`max_disk_bytes`). When
either limit is hit, oldest segments are evicted regardless of cursor
positions. Outputs pointing at evicted segments see a gap (tracked in
`segments_dropped` metric).

**Delivery semantics:** At-least-once when queue is enabled.
Best-effort when queue is `none`.

## Multi-source pipeline

Each source scans independently and writes its own Arrow IPC segments.
At transform time, the pipeline reads the latest segments from each
source and registers them as partitions of the `logs` MemTable.
DataFusion concatenates partitions during query execution.

This means:

- Each source can have a different `ScanConfig` (different wanted_fields
  from predicate pushdown)
- Schema differences between sources are handled by DataFusion's schema
  merging (missing columns are null)
- The StreamingBuilder's `Bytes` lifetime is per-source
- A source with no data in a cycle contributes no partition

## Resource metadata as columns

Source identity and resource attributes are carried as `_resource_*`
prefixed columns (e.g., `_resource_k8s_pod_name`,
`_resource_k8s_namespace`, `_resource_service_name`). These are injected
during scanning based on the source's configuration.

This design:

- Survives SQL transforms naturally (they're just columns)
- Persists in the Arrow IPC segments (always correct, unlike enrichment)
- Enables OTLP output to group rows by resource (group-by on
  `_resource_*` columns, one `ResourceLogs` per distinct combination)
- Uses dictionary encoding for efficiency (same pod name on every row
  from one source costs ~one entry)
- Output sinks exclude `_resource_*` columns from the payload (same
  pattern as `_raw`)

## Column naming conventions

| Prefix | Purpose | Example |
|--------|---------|---------|
| `{field}_str` | String value from JSON | `message_str` |
| `{field}_int` | Integer value from JSON | `status_int` |
| `{field}_float` | Float value from JSON | `latency_float` |
| `_raw` | Raw input line (optional) | `_raw` |
| `_resource_*` | Source/resource metadata | `_resource_k8s_pod_name` |

Type conflicts produce separate columns: `status_int` and `status_str`
can coexist.

## Arrow IPC segment format

**Format:** Arrow IPC File format with atomic seal.

**Write path:** `FileWriter` writes to `.tmp` file. On seal:
`finish()` (writes footer) → `fsync` → atomic `rename` to final path.
Readers never see incomplete files.

**Segment lifecycle:**
1. Pipeline writes batch(es) to `.tmp` file via `FileWriter`
2. At size threshold (64 MB) or time threshold: seal segment
3. On startup: delete orphaned `.tmp` files

**Storage abstraction:** All persistence writes go through a
`SegmentStore` trait with implementations for local filesystem and
object storage (S3/GCS/Azure Blob). The same segment format and code
path is used regardless of storage backend.

## Deployment model

logfwd scales by running more instances. One binary, one pipeline per
instance. S3 is the coordination layer between instances.

**Single instance:** Source → Scanner → Transform → Output, all in one
process. Predicate pushdown works. In-memory fan-out. Simple.

**Scaled out:** Multiple instances with different configs, sharing S3
paths. Each instance is a full logfwd binary.

```
Instance A (edge collector):
  file source → scanner → write Arrow IPC to s3://bucket/raw/

Instance B (central processor):
  s3 source (reads s3://bucket/raw/) → transform → output sinks
  # or: → write to s3://bucket/transformed/ for another instance

Instance C (dedicated sender):
  s3 source (reads s3://bucket/transformed/) → output sinks
```

No custom RPC, no cluster coordination. Arrow IPC on S3 is the
universal interface. Any instance can read any other's segments.

## Scanner architecture

The scanner is the performance-critical path. It has two stages:

**Stage 1 — Chunk classification** (`chunk_classify.rs`): Process the
entire NDJSON buffer in 64-byte blocks. For each block, find all quote
and backslash positions, compute an escape-aware real-quote bitmask, and
build a string-interior mask. Output: `ChunkIndex` with pre-computed
bitmasks.

**Stage 2 — Field extraction** (`scanner.rs`): A scalar state machine
walks top-level JSON objects. For each field, it resolves the key to an
index (HashMap, once per field per batch) and routes the value to the
builder via `append_*_by_idx`. String scanning uses the pre-computed
`ChunkIndex` for O(1) closing-quote lookup.

The scan loop is generic over the `ScanBuilder` trait:

- **`StorageBuilder`**: collects `(row, value)` records. Builds columns
  independently at `finish_batch`. Correct by construction. For
  persistence path (Arrow IPC segments).
- **`StreamingBuilder`**: stores `(row, offset, len)` views into a
  `bytes::Bytes` buffer. Builds `StringViewArray` columns with zero
  copies. 20% faster. For real-time hot path when persistence is
  disabled.

## Design constraints

- **Disk queue before batching:** `disk_queue → batch`, never
  `batch → disk_queue`. The batcher is ephemeral; persistence must
  come first so crash recovery replays from the queue.
- **Dictionary compaction after redaction:** Arrow `filter()` doesn't
  remove unreferenced dictionary values. Any redaction/masking must
  compact dictionaries before export to prevent data leaks.
- **Separate runtimes for data and observability:** Never share a
  single-threaded tokio runtime between the data pipeline and its own
  internal telemetry. On a single-core machine this deadlocks.
- **Admission control before allocation:** Limit inbound data *before*
  reading/deserializing it (semaphore on receiver accept), not after.
  Post-allocation limiting is too late — the memory is already used.

## Async pipeline

The pipeline runs on a tokio multi-thread runtime. Key components:

- **Sources** implement `async fn run(&mut self, ctx: &mut SourceContext)`
  (Arroyo-style source-owns-loop). File sources wrap FileTailer via
  `spawn_blocking`.
- **Scanner** runs on `spawn_blocking` (pure CPU, ~4MB per call).
- **Transform** is `async fn execute()` (DataFusion is natively async).
- **Sinks** implement `async fn send_batch()`. HTTP-based sinks use
  async `reqwest` for connection pooling and timeouts.
- **Shutdown** via `CancellationToken` (already implemented).

## Crate map

| Crate | Purpose |
|-------|---------|
| `logfwd` | Binary. CLI, pipeline orchestration, OTel metrics. |
| `logfwd-core` | Scanner, builders, parsers, diagnostics, enrichment, OTLP encoder. |
| `logfwd-config` | YAML config deserialization and validation. |
| `logfwd-transform` | DataFusion SQL. UDFs: `grok()`, `regexp_extract()`, `int()`, `float()`. |
| `logfwd-output` | Output sinks + async Sink trait. OTLP, JSON lines, HTTP, stdout. |
| `logfwd-bench` | Criterion benchmarks. |

## What's implemented vs not yet

**Implemented:** file receiver (CRI/JSON/Raw parsing, SIMD scanner),
two builder backends (StreamingBuilder default), DataFusion SQL
processor (async), custom UDFs (grok, regexp_extract, int, float,
geo_lookup), enrichment (K8s path, host info, static labels), OTLP
exporter, JSON lines exporter, stdout exporter, diagnostics server,
OTel metrics, signal handling (SIGINT/SIGTERM via CancellationToken),
graceful shutdown, async Sink trait, compliance test suite.

**Not yet:**

*Pipeline core:*
- Async pipeline runtime with bounded channels
- Receiver / Processor / Exporter trait abstractions
- Ack/Nack context stack for end-to-end delivery tracking
- Backpressure (channel + receiver + processor levels)
- Thread-per-core `!Send` local execution
- Component registration via `linkme::distributed_slice`

*Receivers:*
- Arrow IPC (local / S3 / HTTP)
- ClickHouse (`SELECT ... FORMAT ArrowStream`)
- Elasticsearch ES|QL (`?format=arrow`)
- Arrow Flight (gRPC `DoPut`)
- Parquet (local / S3)
- OTLP (protobuf → Arrow)
- CSV / JSON files
- Kafka (Arrow IPC message bytes)
- TCP / UDP / syslog

*Exporters:*
- Arrow IPC files (local / S3, `.arrows` / `.arrow`)
- Parquet files (local / S3)
- ClickHouse (`INSERT ... FORMAT ArrowStream`)
- Arrow Flight (gRPC `DoPut`)
- Kafka (Arrow IPC message bytes)

*Persistence:*
- Arrow IPC segment store (pre/post-transform queue)
- SegmentStore trait (local + S3/GCS backends)
- Output cursor tracking
- File offset checkpointing

*Schema:*
- `_resource_*` column injection
- Timestamp type migration (string → `Timestamp(ns, UTC)`)
- Adaptive dictionary encoding
- OTLP resource grouping in exporter
