# Architecture

## Data flow

```
Sources (file / TCP / UDP / OTLP receiver)
    │
    │  each source produces Bytes independently
    ▼
Format Parser (CRI / JSON / Raw)  ─per source─
    │  strips CRI timestamp/stream prefix, accumulates NDJSON
    ▼
SIMD Scanner  ─per source─
    │  one pass classifies entire buffer via ChunkIndex
    │  walks structural positions directly into Arrow columns
    │  injects _resource_* columns from source metadata
    ▼
Arrow IPC segment (per-source, pre-transform)
    │  written to local disk, uploaded to object storage
    │  THIS is the ack point — source checkpoint advances here
    │
    ├─→ real-time path: register as MemTable partitions
    │   → SQL Transform (DataFusion + enrichment JOINs)
    │   → output sinks (OTLP / JSON lines / HTTP / stdout)
    │
    └─→ object storage: raw Arrow tables
        queryable by DuckDB / Polars / Spark / DataFusion
```

## Pre-transform persistence

Storage is pre-transform, per-source Arrow IPC. Each source writes its
own segments independently. This is the single persistence layer.

**Why pre-transform (not post):**

- Raw data is re-queryable. You can change the SQL transform and
  re-process historical data.
- No duplicate storage. The transform is stateless and re-runnable.
- Arrow IPC on object storage integrates with the entire Arrow
  ecosystem — any query engine can read your log segments directly.
- For the common case (`SELECT * FROM logs`), pre-transform and
  post-transform data are identical. No wasted storage.

**Enrichment tradeoff:** Enrichment data (K8s pod labels, annotations)
is looked up at transform time, not stored with the raw data. On replay,
enrichment reflects current state, not ingest-time state. The
`_resource_*` columns (source identity: pod name, namespace) ARE stored
and are always correct — only enrichment table data goes stale.

## End-to-end ack

Sources must NOT acknowledge to their upstream until the Arrow IPC
segment is durably written (fsync + rename). The ack chain:

```
Scanner produces RecordBatch
  → FileWriter writes to .tmp
  → fsync + rename (segment sealed)
  → Ack source: "data through offset X is durable"
  → Source advances checkpoint
```

Ack latency is scanner + IPC write + fsync, typically 5-15ms. This is
comparable to Kafka's acks=all (3-15ms). For network sources
(TCP/HTTP/OTLP receiver), this is the HTTP response time.

**File sources** don't need the ack latency to be fast — the log file is
already durable. The checkpoint just tracks the file offset.

**Network sources** see the full ack latency. If the transform is slow
(complex GROUP BY), the ack is slow — that's backpressure, and it's
correct behavior.

## Output delivery

Outputs read from the pre-transform Arrow IPC segments, run the
transform, and send. Each output tracks its position independently
(segment + batch offset). A slow output falls behind in the segment
list; it doesn't block other outputs or sources.

**Output cursor tracking:** Each output tracks `{ segment_id, batch_offset }`.
Persisted to a JSON file on each ack. On restart, resume from last
position.

**Retention:** TTL (`max_segment_age`) + size (`max_disk_bytes`). When
either limit is hit, oldest segments are evicted regardless of output
cursor positions. Outputs that were pointing at evicted segments see a
gap (tracked in `segments_dropped` metric).

**New output cursors:** Default to tail (skip history). Configurable
`replay_from: oldest` per output.

**Delivery semantics:** At-least-once. Crash-before-ack means
re-delivery on restart. Monotonic `batch_id` enables optional downstream
deduplication.

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

**Format:** Arrow IPC File format with atomic seal. Each source writes
segments independently.

**Write path:** `FileWriter` writes to `.tmp` file. On seal:
`finish()` (writes footer) → `fsync` → atomic `rename` to final path.
Readers never see incomplete files.

**Segment lifecycle:**
1. Source scans data into RecordBatch
2. `FileWriter` writes batch(es) to `.tmp` file
3. At size threshold (64 MB) or time threshold: seal segment
4. Upload to object storage (async, background)
5. Delete local copy after upload confirmed (or retain per policy)
6. On startup: delete orphaned `.tmp` files

**Object storage:** Segments are uploaded to S3/GCS/Azure Blob after
sealing. Object storage is the primary durability target. Local disk is
a staging area.

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

**Implemented:** file input, CRI/JSON/Raw parsing, SIMD scanner, two
builder backends (StreamingBuilder default), DataFusion SQL transforms
(async), custom UDFs (grok, regexp_extract, int, float), enrichment
(K8s path, host info, static labels), OTLP output, JSON lines output,
stdout output, diagnostics server, OTel metrics, signal handling
(SIGINT/SIGTERM via CancellationToken), graceful shutdown, async Sink
trait.

**Not yet:** async pipeline runtime, async Source trait, pre-transform
Arrow IPC persistence, object storage upload, `_resource_*` column
injection, OTLP resource grouping, output cursor tracking,
TCP/UDP/OTLP input, Elasticsearch/Loki/Parquet output, file offset
checkpointing, SQL rewriter.
