# logfwd Architecture — Context for DataFusion Integration

This document describes the current logfwd architecture in detail so an agent planning the DataFusion integration can understand the data flow, memory model, and optimization opportunities.

## What logfwd Does

logfwd is a high-performance log forwarder. It reads log files (tailing in real time or bulk replay), parses container log format (CRI), extracts/encodes log data, compresses, and ships to a remote endpoint. It runs as a Kubernetes DaemonSet.

Current throughput on a single core (arm64):
- Raw CRI parse: 26M lines/sec
- JSON lines + zstd: 6M lines/sec
- OTLP protobuf + zstd (with JSON field extraction): 4.7M lines/sec
- OTLP protobuf + zstd (raw body, no JSON parse): 5.3M lines/sec

## Data Flow

```
Disk file
  │
  ▼
Read buffer (1MB, heap-allocated, reused)
  │
  ▼
CRI parser ─── extracts message from CRI envelope ──▶ output buffer
  │                 (zero-copy for full lines,
  │                  copies only for partial line reassembly)
  │
  ▼
Output buffer (4MB, heap-allocated, reused)
  │  Contains newline-delimited messages extracted from CRI.
  │  References point into this buffer for subsequent processing.
  │
  ├─── JSON lines path: add field prefix, compress, send
  │
  └─── OTLP path: scan JSON fields, encode protobuf, compress, send
```

## Memory Model

### Buffer Lifecycle

The system uses a **double-buffer swap** pattern (see `chunk.rs`):

1. **Fill buffer**: Being filled by `read()` syscalls.
2. **Process buffer**: Owned by the processing pipeline (CRI parse → encode → compress).
3. On swap: the filled buffer becomes the process buffer, and vice versa. Only the leftover partial line (~200 bytes) is copied between them.

All `LogEvent` references (field positions, body slices) point into the process buffer. They are valid until the buffer is swapped back to the reader. The pipeline must finish encoding before the swap.

### Current Per-Line Memory Cost

In the OTLP-with-field-extraction path, per-line work includes:
- JSON field scanning: 3+ memchr calls to find quote positions, match key names against known lists (timestamp, level, message). All results are `(offset, len)` into the buffer — no allocation.
- Timestamp parsing: Hand-rolled ISO 8601 parser, ~15ns. Pure arithmetic, no allocation.
- Severity parsing: Byte comparison, lookup table. ~1ns.
- Protobuf encoding: Writes varint tags + lengths + data into a reusable `Vec<u8>`. The only "copy" is the field values being written into the protobuf output.

Total: ~125ns per line. 0.03 heap allocations per line (essentially zero — the 30K allocations across 1M lines are from buffer growth during the first batch, then everything is reused).

### What Is NOT in the Event Model Yet

There is currently **no materialized event struct** per log line. The OTLP encoder (`otlp.rs`) calls `extract_json_fields()` which returns a `JsonFields` struct with `Option<&[u8]>` references, uses them immediately for encoding, and they go out of scope. Nothing is stored per-line.

This is both a strength (no per-line allocation) and a limitation (no way to evaluate complex predicates across fields of the same line without re-scanning).

## The JSON Field Scanner (`otlp.rs`)

The current scanner (`extract_json_fields`) is hard-coded to look for three categories of keys:

```rust
const TIMESTAMP_KEYS: &[&[u8]] = &[b"timestamp", b"time", b"ts", b"@timestamp", b"datetime", b"t"];
const LEVEL_KEYS: &[&[u8]] = &[b"level", b"severity", b"log_level", b"loglevel", b"lvl"];
const MESSAGE_KEYS: &[&[u8]] = &[b"message", b"msg", b"body", b"log", b"text"];
```

It scans the JSON line using memchr to find quote characters, extracts key-value pairs, and matches keys against these lists (case-insensitive). It stops early once all three fields are found.

### What the Scanner Does NOT Do

- Does not handle escaped quotes in keys (rare in log data)
- Does not handle nested JSON objects as values (treats them as opaque bytes)
- Does not extract arbitrary fields — only the three hardcoded categories
- Does not build any index or offset table of all fields

## CRI Parser (`cri.rs`)

Kubernetes container runtimes write logs in CRI format:
```
2024-01-15T10:30:00.123456789Z stdout F {"level":"INFO","msg":"hello"}
^timestamp                     ^stream ^flag ^message
```

The parser:
1. Finds the 3 space delimiters using memchr (fast)
2. Extracts the message portion (everything after the third space)
3. Handles partial lines (flag "P") by buffering and reassembling

For full lines (flag "F", the common case), the message references bytes directly in the input read buffer — zero copy. Partial lines copy into an internal reassembly buffer.

Output: `process_cri_to_buf()` writes extracted messages into a contiguous output buffer with optional JSON prefix injection (for adding `kubernetes.pod_name` etc.).

## OTLP Protobuf Encoder (`otlp.rs`)

Hand-rolled protobuf encoder. No prost, no codegen. The wire format is:

```
ExportLogsServiceRequest
  └─ ResourceLogs
       └─ ScopeLogs
            └─ repeated LogRecord
                 ├─ time_unix_nano (fixed64)
                 ├─ severity_number (varint)
                 ├─ severity_text (string)
                 ├─ body: AnyValue { string_value } (string)
                 └─ observed_time_unix_nano (fixed64)
```

Two encoding modes:
- **`encode_log_record()`**: Scans JSON for timestamp/severity/message, maps to OTLP fields. 100ns/line.
- **`encode_log_record_raw()`**: Entire line becomes body string, observed_time only. 28ns/line.

`BatchEncoder` holds reusable buffers (`records_buf`, `record_ranges`, `out`) across batches. `encode_from_buf()` takes a contiguous newline-delimited buffer and encodes all lines without requiring a `Vec<&[u8]>` intermediate.

### Two-Pass Encoding

The encoder currently makes two passes:
1. Encode all LogRecords into `records_buf`, tracking `(start, end)` offsets.
2. Compute total sizes (needed for protobuf length-delimited wrappers), write the ExportLogsServiceRequest wrapper + copy records into final output.

This is because protobuf requires knowing the total size of a length-delimited message before writing the length prefix. A single-pass approach would require either reserving max-size length fields or backpatching, which we haven't implemented.

## Threading Model

### Daemon Mode (`daemon.rs`)

```
Main thread (reader):
  loop:
    discover_files(glob_pattern)
    for each file:
      read() → CRI parse → JSON inject → accumulate in batch buffer
      when batch is full: push to crossbeam channel
    sleep if no data

4x Sender threads:
  loop:
    receive batch from channel
    HTTP POST to endpoint
```

The reader never blocks on network. Bounded channel (32 slots) provides backpressure. When the channel is full, the reader blocks — this is intentional.

Profiling with the two-thread split:
- read: 34%, cri_parse: 20%, channel_send: 46% (at peak throughput)

### Pipeline Mode (pipeline.rs — for benchmarking)

Single-threaded. Uses the double-buffer chunk accumulator. Reads from a file, processes chunks, compresses (or encodes to OTLP), counts stats. No networking.

## What DataFusion Integration Needs

### The Core Opportunity: Predicate Pushdown to the Scanner

Currently, the JSON scanner extracts 3 hardcoded field categories. With DataFusion, the query layer can tell the scanner which fields it actually needs:

```sql
SELECT timestamp, level, service FROM logs WHERE level = 'ERROR'
```

Pushdown hints to the scanner:
1. **Field selection**: Only extract `timestamp`, `level`, `service` — skip all other JSON keys.
2. **Predicate**: Only emit lines where `level = "ERROR"` — skip non-matching lines entirely.

This reduces:
- CPU: Fewer fields to extract, fewer bytes to copy.
- Memory: Extracted events are smaller (3 fields vs all fields).
- Network: Fewer log records shipped if predicate filters lines.

### The Event Model Question

There is currently no per-line event struct in the hot path. DataFusion would need one to evaluate predicates. The proposed model:

```rust
struct LogEvent<'a> {
    raw: &'a [u8],                           // original line, always available
    timestamp_ns: u64,                       // parsed if requested
    severity: Severity,                      // parsed if requested
    message: Option<&'a [u8]>,               // extracted if requested
    attributes: SmallVec<[KV<'a>; 4]>,       // only requested fields
}

struct KV<'a> {
    key: &'a [u8],
    value: &'a [u8],
}
```

All references point into the read/process buffer. Zero per-line allocation for up to 4 attributes (SmallVec inline storage). The event is ~64-96 bytes on the stack.

The key design question for DataFusion: **does it need a materialized Arrow RecordBatch per chunk, or can it evaluate predicates from a streaming scan?**

- If RecordBatch: We need to accumulate ~4000 events (one chunk's worth), build Arrow arrays (one column per extracted field), pass to DataFusion for filter evaluation, then encode the surviving rows.
- If streaming: We evaluate the predicate inline in the JSON scanner callback, emit only matching lines, no Arrow needed.

The streaming approach is faster (no Arrow array construction) but limits DataFusion to simple predicates (equality, comparison, existence). The RecordBatch approach enables full SQL expressiveness (JOIN, aggregation, UDFs) at the cost of materialization.

### String vs JSON Detection

Lines starting with `{` are treated as JSON. Everything else is a plain string log. For string logs:
- Body = entire line
- Attributes = empty (no fields to extract)
- Timestamp = attempt to parse from line prefix, fall back to observed time
- Severity = attempt to detect level keywords, fall back to unspecified

For JSON logs:
- Body = message field (if found) or full line
- Attributes = all other JSON fields (when extracting all), or only requested fields (with pushdown)
- Timestamp = from timestamp field
- Severity = from level field

### Where DataFusion Would Plug In

```
Read buffer
  │
  ▼
CRI parser → raw message bytes
  │
  ▼
[DataFusion FieldExtractor] ← receives pushdown hints from query plan
  │  - scans JSON for requested fields only
  │  - evaluates predicates inline
  │  - emits LogEvent or nothing (filtered out)
  │
  ▼
OTLP encoder ← consumes LogEvent, writes protobuf
  │
  ▼
Compressor → Network
```

The FieldExtractor replaces the current `extract_json_fields()` function. It's configured at query plan compilation time with the set of needed fields and filter predicates. At runtime it's a tight loop: scan JSON, check predicates, emit or skip.

### Resource vs Log Attributes

Per OTLP spec and how receivers (Loki, Datadog, Elastic) handle data:
- **Resource attributes**: Identity of the source. Constant per file/pod. Set once. Examples: `service.name`, `k8s.pod.name`, `k8s.namespace.name`.
- **Log record attributes**: Per-line structured fields extracted from JSON. Examples: `trace_id`, `user_id`, `request_id`, `http.method`.
- **Body**: Human-readable message string. For JSON logs, ideally the `message`/`msg` field value. For string logs, the full line.

Currently logfwd puts the pod name via HTTP header (`VL-Extra-Fields`), not in the protobuf. A proper implementation would set resource attributes in the ResourceLogs wrapper.

### Compression Considerations

The OTLP-zstd path achieves 82.6x compression ratio (1GB JSON → 12MB protobuf+zstd). The raw-body path gets 17x. The difference is because extracted protobuf fields (repeated field tags, varint-encoded timestamps) compress much better than raw JSON strings.

With predicate pushdown filtering out lines, less data flows through compression, directly reducing network bandwidth.

## File Layout for Reference

| File | Lines | Purpose |
|------|-------|---------|
| `chunk.rs` | 307 | Double-buffer accumulator |
| `compress.rs` | 195 | zstd compression + wire format |
| `cri.rs` | 292 | CRI parser + reassembler |
| `daemon.rs` | 345 | K8s daemon mode (reader + sender threads) |
| `e2e_bench.rs` | 241 | E2E benchmark harness |
| `main.rs` | 541 | CLI entrypoint |
| `otlp.rs` | 726 | OTLP protobuf encoder + JSON scanner |
| `pipeline.rs` | 337 | Chunk pipeline (for benchmarking) |
| `read_tuner.rs` | 215 | Read buffer auto-tuner |
| `sender.rs` | 105 | HTTP sender |
| `tail.rs` | 618 | File tailer |
| `tuner.rs` | 570 | Chunk size auto-tuner |
| **Total** | **4,503** | |
