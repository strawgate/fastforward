# logfwd Evolution Plan

## Where We Are

4,503 lines of Rust. 4.7M lines/sec OTLP output. 125ns/line. Zero allocations.
The architecture is a straight pipeline: read → CRI parse → JSON scan → protobuf encode → compress → send.

The speed comes from three things we must preserve:
1. Zero-copy byte slice references into a reusable read buffer
2. No per-line heap allocation (SmallVec/stack for field refs)
3. No intermediate data structure — scanner output feeds directly to encoder

## Where We're Going

```
INPUTS                    TRANSFORM              OUTPUTS
─────────                 ─────────              ───────
File tail (CRI)     ─┐                      ┌─  OTLP gRPC/HTTP
File tail (raw)      │    SQL (DataFusion)   │   Elasticsearch bulk
UDP syslog           ├──► or                ─┤   JSON lines HTTP
TCP stream           │    Fast path inline   │   Loki
OTLP gRPC receiver   │                      │   Stdout
OTLP HTTP receiver  ─┘                      └─  File / S3
```

## The Seven Things We Need To Figure Out

### 1. The Common Event Representation

This is the central design question. Currently there's no event struct in the hot path.
The scanner returns `JsonFields` with `Option<&[u8]>` references and they're consumed
immediately. We need a representation that:

- All inputs can produce
- The SQL layer can consume (Arrow RecordBatch)
- The fast path can bypass (zero-copy references)
- All outputs can serialize from
- Preserves original types from JSON parsing

**The answer from our conversation**: Two parallel paths sharing one representation.

```
Input produces a "LineBatch": a contiguous buffer of complete log messages
plus metadata (source, format, timestamps).

FAST PATH: scanner extracts fields as (offset, len, type_tag) refs
           → predicate evaluation inline
           → encoder consumes refs directly
           → 150-200ns/line

SQL PATH:  scanner extracts ALL fields as (offset, len, type_tag) refs
           → build typed Arrow columns (duration_ms_int, level_str, etc.)
           → DataFusion executes cached plan
           → output serializer reads result + original typed columns
           → 800-1500ns/line
```

The key realization: the scanner already does the work of finding field positions.
The only difference between fast path and SQL path is whether we MATERIALIZE those
positions into Arrow column arrays or consume them inline.

**Research needed:**
- Benchmark: how much does Arrow column construction cost on top of scanning?
  The scanner currently costs ~100ns/line for 3 fields. Building Arrow StringArray
  from pre-computed (offset, len) refs should be ~50-100ns per field per line
  (just memcpy the value bytes into the StringBuilder). So 10 fields × 8192 lines
  would add ~500-1000ns/line. That's the real overhead of the SQL path.
- Prototype: extend extract_json_fields to return ALL field positions (not just 3),
  measure the scanning cost increase. Currently it early-exits after finding
  timestamp+level+message. A full scan continues to EOF of each line.

### 2. Input Abstraction

Currently the pipeline is driven by file reading. The read buffer, CRI parser,
and chunk accumulator are tightly coupled. We need to decouple "where bytes come from"
from "how bytes are processed."

**Proposed interface:**

```rust
/// What every input produces: a batch of complete log messages in a contiguous buffer.
struct MessageBatch {
    /// Contiguous buffer of newline-delimited messages.
    /// For file inputs: CRI-stripped messages.
    /// For UDP/TCP: protocol-framed messages.
    /// For OTLP: deserialized LogRecord bodies (may need re-encoding!).
    buf: Vec<u8>,
    /// Per-message metadata. Parallel to messages.
    /// Includes: byte offset in buf, source identity, receive timestamp.
    messages: Vec<MessageMeta>,
}

struct MessageMeta {
    offset: u32,      // start position in buf
    len: u32,         // message length
    timestamp_ns: u64, // receive/observed timestamp
    source_id: u16,   // which input source (for routing/labeling)
}
```

**Input implementations:**

| Input | Notes | Priority |
|-------|-------|----------|
| File tail (CRI) | Exists. Wrap chunk accumulator output. | P0 — already done |
| File tail (raw) | Exists partially. Skip CRI parse stage. | P0 |
| UDP syslog | `tokio::net::UdpSocket`, recv_batch. Each datagram = one message. Lock-free: each recv goes into a thread-local buffer. | P1 |
| TCP stream | `tokio::net::TcpListener`. Frame by newline or length prefix. Need backpressure (don't accept faster than we can process). | P1 |
| OTLP gRPC | `tonic` server. Deserialize ExportLogsServiceRequest, extract LogRecord bodies. NOTE: this is the reverse of our encoder — we'd be decoding protobuf into our MessageBatch format. | P2 |
| OTLP HTTP | `hyper` server. Same as gRPC but JSON or protobuf body. | P2 |

**The OTLP input problem**: When we receive OTLP, the data is ALREADY structured
(timestamp, severity, body, attributes are separate protobuf fields). Converting it
back to a flat text message just to re-parse it is wasteful. We should be able to
go OTLP → Arrow columns directly, bypassing the JSON scanner entirely.

This suggests the MessageBatch abstraction might be wrong — or at least that we need
a "pre-parsed" variant:

```rust
enum InputBatch {
    /// Raw message bytes. Need scanning/parsing.
    Raw(MessageBatch),
    /// Already-structured data (from OTLP input, Elasticsearch input, etc.).
    /// Can go directly to Arrow columns or to output serializers.
    Structured(RecordBatch),
}
```

**Research needed:**
- What's the actual throughput of `recvmmsg` on UDP vs our file read path?
  UDP syslog at 100K msgs/sec × 200 bytes = 20 MB/sec. That's well within
  what a single core can handle. The bottleneck will be the scanner, not the network.
- For TCP: what framing protocol? Syslog uses octet counting (RFC 5425) or
  newline-delimited. Fluent Forward protocol uses msgpack. Do we need to support
  Fluent Forward for compatibility with Fluent Bit deployments?
- For OTLP input: tonic server performance. Can a single tonic server handle
  100K LogRecords/sec? (Probably yes — tonic is fast.) The real question is
  whether protobuf deserialization + Arrow construction is faster or slower
  than our file read + CRI parse + JSON scan path.

### 3. The SQL Layer Integration

We designed this extensively in the conversation. Summary of what's settled:

- All columns are Utf8 by default
- Multi-typed columns when type detection is enabled (duration_ms_int, duration_ms_str)
- Schema inferred from first batch + SQL column references
- Plan compiled once, recompiled on schema extension (~1ms, rare)
- Rewriter maps user column names to typed column variants
- `int()`, `float()` functions for explicit type casting
- COALESCE across typed variants for bare column references
- `SELECT * EXCEPT`, `REPLACE` work on real columns
- Fast path auto-detection: SQL parser determines if query is simple enough
  to evaluate inline in the scanner (no DataFusion needed)

**What's NOT settled:**

a) **Buffer lifecycle coordination.** The double-buffer swap in chunk.rs gives the
   processing side exclusive ownership of the buffer via OwnedChunk. DataFusion
   needs to complete before we reclaim the buffer. Current chunk processing
   (encode + compress) takes ~1ms for 4000 lines. DataFusion at 1000ns/line
   would take ~4ms for 4000 lines. That's 4x longer holding the buffer.
   
   Options:
   - Increase chunk size to amortize (already tunable, tuner may converge higher)
   - Copy relevant bytes out of the buffer into Arrow StringBuilders (the memcpy
     happens anyway when building Arrow arrays, so no extra cost)
   - Pipeline the buffer release: build Arrow columns, release buffer, THEN execute
     DataFusion. The Arrow arrays own their data independently of the read buffer.

   **Recommendation:** Option 3. The Arrow column construction step copies field
   values from the read buffer into Arrow StringBuilders. Once construction is
   complete, the read buffer can be reclaimed. DataFusion execution then operates
   on the Arrow RecordBatch which owns its own memory. This decouples DataFusion
   latency from buffer swap timing.

b) **Where does the scanner get its field list?** Currently hardcoded. With SQL,
   the field list comes from parsing the query at startup. But `SELECT *` means
   "all fields" — the scanner needs to extract everything. For the fast path
   (simple filter), the scanner only needs the filter field(s).
   
   The scanner should accept a `ScanConfig`:
   ```rust
   enum ScanMode {
       /// Extract only these fields (for fast path or selective SQL queries)
       Selective(Vec<FieldSpec>),
       /// Extract all fields (for SELECT *)
       All,
   }
   
   struct FieldSpec {
       name: Vec<u8>,         // field name to look for
       aliases: Vec<Vec<u8>>, // alternate names (ts, timestamp, @timestamp)
   }
   ```

c) **The rewriter implementation.** We need `sqlparser-rs` to parse the user's SQL,
   walk the AST, and rewrite column references. This is ~500-800 lines of Rust.
   The rewriter needs access to the field-type map (which fields exist, which have
   _int/_str/_float variants). The map is populated after the first batch is scanned.
   
   **Startup sequence:**
   1. Parse SQL, extract column references (known before any data arrives)
   2. Wait for first batch
   3. Scan first batch, discover field names and types
   4. Build field-type map: merge SQL refs + discovered fields
   5. Run rewriter against the map to produce the internal SQL
   6. Compile DataFusion plan against the typed schema
   7. Cache plan, process first batch and all subsequent batches

d) **How does `SELECT *` work with multi-typed columns?** If `duration_ms` has both
   `_int` and `_str` variants, `SELECT *` returns both. The rewriter maps `*` to
   all columns, and for display/output the serializer coalesces:
   COALESCE(CAST(duration_ms_int AS VARCHAR), duration_ms_str) as duration_ms.
   For output serialization (OTLP), check _int first, then _str per row.

### 4. Output Abstraction

Currently the output is either JSON lines bytes or OTLP protobuf bytes, both
compressed and sent via HTTP POST. We need to support multiple simultaneous
outputs with different formats and destinations.

**Proposed interface:**

```rust
trait OutputSink: Send {
    /// Receive a processed batch. The sink serializes and sends it.
    /// For the fast path: batch contains raw line refs.
    /// For the SQL path: batch contains a RecordBatch + original typed columns.
    fn send(&mut self, batch: &OutputBatch) -> io::Result<()>;
    
    /// Flush any buffered data (for graceful shutdown).
    fn flush(&mut self) -> io::Result<()>;
}

enum OutputBatch {
    /// Fast path: filtered raw lines, ready for memcpy or encoding.
    RawLines {
        buf: &[u8],
        line_offsets: &[(usize, usize)],  // (start, len) of each surviving line
        field_refs: &[FieldRefs],         // per-line field positions for OTLP
    },
    /// SQL path: DataFusion result + original typed columns for type preservation.
    Transformed {
        result: RecordBatch,              // DataFusion output
        typed_columns: TypedColumnIndex,  // original _int/_str/_float columns
        touched_columns: HashSet<String>, // columns the SQL actively operated on
    },
}
```

**Output implementations:**

| Output | Serialization | Batching | Priority |
|--------|--------------|----------|----------|
| OTLP gRPC | Hand-rolled protobuf (existing) | Batch by ResourceLogs grouping | P0 |
| OTLP HTTP | Same protobuf, different transport | Same | P0 |
| JSON lines HTTP | Write field values as JSON objects | Batch by size/time | P0 |
| Elasticsearch | JSON + bulk API action lines | Batch by index | P1 |
| Loki | Protobuf, group by label set, snappy compress | Batch by stream | P1 |
| File | Write to local file (for debug/archival) | Flush on size/time | P1 |
| Stdout | Write to stdout (for piping) | Line-buffered | P0 |
| S3/GCS | Parquet or JSON, upload on size/time | Buffer to temp file | P2 |

**The fan-out problem:** Multiple outputs from one pipeline. After the transform
(fast path or SQL), the result needs to go to N outputs simultaneously. Options:
- Sequential: serialize for output 1, then output 2, etc. Simple but slow if
  multiple outputs need different serializations.
- Parallel: each output runs in its own thread/task, receives a shared reference
  to the batch. The batch stays alive until all outputs have consumed it.
  This is what the existing crossbeam channel architecture supports naturally —
  broadcast the batch to multiple channels.

**The OTLP output enhancement:** The existing hand-rolled protobuf encoder is
one of logfwd's biggest performance advantages. We need to preserve it for the
fast path AND add a RecordBatch-based encoder for the SQL path.

```rust
impl OtlpSink {
    fn send(&mut self, batch: &OutputBatch) -> io::Result<()> {
        match batch {
            OutputBatch::RawLines { buf, line_offsets, field_refs } => {
                // Existing fast path: encode_from_buf()
                // Use the hand-rolled encoder directly from byte refs
                self.encoder.encode_from_refs(buf, line_offsets, field_refs)
            }
            OutputBatch::Transformed { result, typed_columns, .. } => {
                // SQL path: walk Arrow columns, write protobuf
                // For untouched columns, read original types from typed_columns
                // For touched columns, read types from result schema
                self.encoder.encode_from_batch(result, typed_columns)
            }
        }
    }
}
```

**Research needed:**
- Elasticsearch bulk API batching: what's the optimal batch size? ES recommends
  5-15 MB per bulk request. With ~200 bytes/line, that's 25-75K lines per batch.
  We may need to accumulate multiple pipeline batches before flushing to ES.
- Loki push API: requires grouping logs by label set (stream). Different from
  our "all lines in one batch" model. Need a grouping stage per output.
- For multi-output fan-out: benchmark the overhead of Arc<RecordBatch> shared
  across multiple output threads vs cloning/serializing sequentially.

### 5. The Configuration Model

Currently CLI args. Need a proper config file for production deployment.

```toml
# /etc/logfwd/config.toml

# Global settings
[global]
log_level = "info"
metrics_port = 9090        # Prometheus metrics endpoint
data_dir = "/var/lib/logfwd"  # checkpoint storage

# Input: tail Kubernetes pod logs
[[input]]
type = "file"
path = "/var/log/pods/**/*.log"
format = "cri"                  # cri | json | logfmt | syslog | raw | auto
multiline.pattern = "^\\d{4}-"  # optional: merge continuation lines

# Input: receive syslog over UDP
[[input]]
type = "udp"
listen = "0.0.0.0:514"
format = "syslog"

# Input: receive OTLP
[[input]]
type = "otlp"
grpc_listen = "0.0.0.0:4317"
http_listen = "0.0.0.0:4318"

# Transform: SQL query
# Applied to all inputs unless input-specific routing is configured
[transform]
sql = """
  SELECT * EXCEPT (stack_trace, debug_context)
         REPLACE (redact(message, 'email') as message),
         'production' as environment
  FROM logs
  WHERE level != 'DEBUG'
"""

# Output: forward to OTLP collector
[[output]]
type = "otlp"
endpoint = "otel-collector:4317"
protocol = "grpc"           # grpc | http
compression = "zstd"        # zstd | gzip | none
resource_attributes = { "service.name" = "logfwd", "k8s.cluster.name" = "prod" }

# Output: archive to Elasticsearch
[[output]]
type = "elasticsearch"
urls = ["https://es-1:9200", "https://es-2:9200"]
index = "logs-{service}-{date}"
bulk_max_bytes = 5_242_880  # 5MB per bulk request
username = "elastic"
password_env = "ES_PASSWORD"

# Output: debug to stdout (optional)
[[output]]
type = "stdout"
format = "json"
```

**Input routing:** By default, all inputs feed into one transform and all outputs
receive the result. For more complex setups, explicit routing:

```toml
[[input]]
type = "file"
path = "/var/log/app/*.log"
format = "json"
route = "app_pipeline"

[[input]]
type = "udp"
listen = "0.0.0.0:514"
format = "syslog"
route = "syslog_pipeline"

[[pipeline]]
name = "app_pipeline"
sql = "SELECT * FROM logs WHERE level != 'DEBUG'"
outputs = ["otlp_main", "es_archive"]

[[pipeline]]
name = "syslog_pipeline"
sql = "SELECT * FROM logs"
outputs = ["otlp_main"]

[[output]]
name = "otlp_main"
type = "otlp"
endpoint = "collector:4317"

[[output]]
name = "es_archive"
type = "elasticsearch"
urls = ["https://es:9200"]
```

This is the "one pipeline per log type" model from our conversation, expressed
as config rather than separate processes.

### 6. Backpressure, Buffering, and Delivery Guarantees

Currently: bounded crossbeam channel (32 slots) between reader and sender.
Reader blocks when channel is full. Simple and effective.

For production with multiple inputs and outputs, we need:

a) **Per-output buffering.** If ES is slow but OTLP is fast, don't slow down OTLP
   because ES is backed up. Each output gets its own buffer/channel.

b) **Disk-backed overflow.** When an output's in-memory buffer is full, spill to disk.
   On recovery, replay from disk. This is critical for at-least-once delivery.
   
   Design: write batches to a WAL (append-only file) before sending. Advance a
   checkpoint after successful send acknowledgment. On restart, replay from last
   checkpoint. The WAL uses the existing compress.rs wire format — it's already
   a size-prefixed compressed chunk format.

c) **Backpressure to inputs.** File inputs handle backpressure naturally (just stop
   reading). UDP inputs drop datagrams (acceptable for syslog). TCP inputs stop
   reading from the socket (TCP backpressure propagates to sender). OTLP inputs
   return gRPC errors (the sender retries).

d) **End-to-end acknowledgments.** For file inputs: checkpoint the file offset after
   all outputs have acknowledged the batch. Currently daemon.rs doesn't checkpoint
   at all — it starts from the beginning or end of each file on restart.

**Research needed:**
- WAL implementation: do we build our own or use an existing crate? `sled` is heavy.
  A simple append-only file with the existing compress.rs format might be sufficient.
  Measure: how fast can we write 10MB compressed chunks to disk? SSD sequential
  write is ~500 MB/sec, so this isn't a bottleneck.
- Checkpoint format: per-input file offset + per-output WAL position. Simple JSON
  file, atomically written via rename. The current tail.rs already tracks offsets
  via FileIdentity — extend this to persist across restarts.

### 7. Kubernetes Integration

Currently: DaemonSet daemon.rs reads from glob pattern, injects pod name via
JSON prefix. This works but is limited:

- Pod name is extracted from the file path (brittle)
- No namespace, labels, annotations, container name
- No dynamic pod discovery (just glob matching)
- Resource attributes are sent via HTTP header, not in protobuf

**What we need:**
- Kubernetes API watcher for pod metadata (labels, annotations, namespace)
- Cache pod metadata by pod UID (shared across all pipeline instances)
- Inject metadata as OTLP resource attributes (not in log body)
- Dynamic pipeline configuration from pod annotations
  (e.g., `logfwd.io/format: json`, `logfwd.io/transform: "SELECT * WHERE level != 'DEBUG'"`)
- Per-pod log discovery under /var/log/pods/{namespace}_{pod}_{uid}/{container}/

**Research needed:**
- kube-rs client library: memory footprint of the watch API. We need a
  long-running watch on pods, which maintains a local cache. kube-rs's
  runtime::watcher does this. Measure: how much memory for 200 pods?
- Can we use the Kubernetes downward API (mounted files) instead of API calls
  for the common case? The downward API gives us pod name, namespace, labels
  as files. No API client needed. Limitation: only for the pod logfwd runs in,
  not for other pods on the node. So we need the API client for DaemonSet mode.

## Build Order

### Phase 0: Configurable Scanner (2-3 weeks)

Extend extract_json_fields to accept a configurable field list and inline predicates.
This is pure extension of existing code — no new abstractions.

Deliverables:
- [ ] ExtractConfig struct: field names to extract, simple predicates
- [ ] extract_json_fields_configured(): scans for configured fields, evaluates predicates inline
- [ ] Benchmark: scanning cost for 3, 6, 12, all fields
- [ ] Benchmark: inline predicate evaluation cost
- [ ] TOML config file parser (basic: source path, format, filter, extract fields, output endpoint)
- [ ] Wire into daemon.rs: replace hardcoded fields with config

Expected performance: 3-4M lines/sec (vs 4.7M today for 3 fields)

### Phase 1: Output Abstraction (2 weeks)

Decouple output serialization from the pipeline. Multiple outputs from one pipeline.

Deliverables:
- [ ] OutputSink trait
- [ ] OtlpSink (wraps existing encoder + sender)
- [ ] JsonLinesSink (wraps existing JSON path + sender)
- [ ] StdoutSink (for debugging)
- [ ] Fan-out: broadcast batch to multiple sinks
- [ ] Per-output crossbeam channel + sender thread
- [ ] Config: multiple [[output]] sections

### Phase 2: Input Abstraction (2 weeks)

Decouple input from file-specific code. Support UDP and TCP.

Deliverables:
- [ ] InputSource trait → produces MessageBatch
- [ ] FileInput (wraps existing tail.rs + cri.rs + chunk.rs)
- [ ] UdpInput (syslog receiver)
- [ ] TcpInput (newline-delimited stream)
- [ ] Config: multiple [[input]] sections
- [ ] Input routing to named pipelines

### Phase 3: SQL Transform — Column Construction (3 weeks)

Build the Arrow RecordBatch from scanner output. This is the bridge between
the existing zero-copy scanner and DataFusion.

Deliverables:
- [ ] FieldScanner: full-document JSON scanner that returns all field positions + type tags
- [ ] RecordBatchBuilder: takes scanner output, builds typed Arrow columns
      (duration_ms_int, duration_ms_str, level_str, etc.)
- [ ] Schema inference: first batch discovers fields, subsequent batches reuse/extend
- [ ] Benchmark: column construction cost on top of scanning
- [ ] _raw column: original line bytes for passthrough
- [ ] Type tag tracking: record whether each value was string/int/float/bool/nested in JSON

### Phase 4: SQL Transform — DataFusion Integration (3-4 weeks)

The SQL execution layer. This is the most complex phase.

Deliverables:
- [ ] SQL parser integration (sqlparser-rs): extract column references from user SQL
- [ ] Query analyzer: determine if query is "simple enough" for fast path
- [ ] SQL rewriter: map bare column names to typed column variants
      (duration_ms → COALESCE(CAST(duration_ms_int AS VARCHAR), duration_ms_str))
- [ ] int(), float(), timestamp(), redact() UDF registration
- [ ] DataFusion SessionContext setup with typed schema
- [ ] Plan compilation and caching
- [ ] Plan recompilation on schema extension
- [ ] Execution: RecordBatch → DataFusion → result RecordBatch
- [ ] Integration with output sinks: OutputBatch::Transformed path

### Phase 5: Output Serializer Enhancement (2 weeks)

Output serializers that handle both fast-path and SQL-path data, with type preservation.

Deliverables:
- [ ] OTLP encoder from RecordBatch (encode_from_batch): walks Arrow columns,
      uses typed column suffixes for correct protobuf attribute types
- [ ] Untouched-column type preservation: columns not referenced in SQL use
      original typed columns for OTLP serialization
- [ ] Elasticsearch bulk encoder from RecordBatch
- [ ] JSON lines encoder from RecordBatch
- [ ] Benchmark: SQL-path output serialization cost

### Phase 6: Kubernetes Integration (2-3 weeks)

Proper Kubernetes metadata enrichment.

Deliverables:
- [ ] kube-rs pod watcher with local cache
- [ ] OTLP resource attributes from pod metadata (not in log body)
- [ ] Dynamic pipeline config from pod annotations
- [ ] Per-pod log file discovery
- [ ] Proper /var/log/pods/ path parsing for namespace/pod/container

### Phase 7: Durability (2-3 weeks)

Disk-backed buffering and at-least-once delivery guarantees.

Deliverables:
- [ ] WAL writer: append compressed batches to disk
- [ ] WAL reader: replay on startup
- [ ] Per-input file offset checkpointing
- [ ] Per-output WAL position tracking
- [ ] Graceful shutdown: flush all buffers, write final checkpoint

## Key Research Tasks (Do Before Building)

### R1: Full-document JSON scanning cost

Extend extract_json_fields to extract ALL fields (don't early-exit after 3).
Measure ns/line for 5, 10, 15, 20 fields. This tells us the real cost of SELECT *.

### R2: Arrow column construction from pre-scanned positions

After the scanner finds all field positions as (offset, len, type_tag), how fast
can we build the Arrow RecordBatch? Benchmark: memcpy field values into StringBuilders,
build typed columns, produce RecordBatch. This is the key number for the SQL path.

### R3: DataFusion cold start

Time from empty SessionContext → register RecordBatch → compile SQL plan → first
execution. This is the startup latency users will feel. Target: <50ms.

### R4: Rewriter complexity

Prototype the SQL rewriter with sqlparser-rs. Test against 20 real-world SQL
transforms to find edge cases. The rewriter is the UX-critical piece — if it
produces wrong SQL or confusing errors, the product fails.

### R5: Multi-output fan-out overhead

Benchmark: one transform result → serialize to OTLP + serialize to JSON lines
+ serialize to ES bulk, all on separate threads receiving shared &RecordBatch.
What's the throughput vs single-output?

### R6: UDP/TCP receive throughput

Benchmark: UDP recvmmsg at 100K, 500K, 1M msgs/sec. TCP accept + read at
similar rates. What's the per-message overhead of our input abstraction?

### R7: OTLP-to-OTLP passthrough

When the input is OTLP gRPC and the output is OTLP gRPC (common in collector
deployments), can we short-circuit and avoid deserialize→Arrow→serialize?
Measure the cost of the full path vs a raw protobuf passthrough.

## Risk Register

| Risk | Impact | Mitigation |
|------|--------|------------|
| SQL rewriter has edge cases that produce wrong queries | Users get wrong results silently | Extensive test suite against 50+ SQL patterns. Fallback: if rewriter fails, show error with original and rewritten SQL for debugging. |
| DataFusion version upgrades break our integration | Build fails or behavior changes | Pin DataFusion version. Contribute upstream fixes. Minimize API surface used. |
| Arrow column construction makes SQL path too slow | Can't compete on benchmarks | The fast path exists for simple cases. SQL path only needs to beat Vector (25K/sec), not our own fast path. |
| Multi-output fan-out creates memory pressure | OOM under load | Per-output backpressure with bounded buffers. Disk overflow for slow outputs. |
| Schema extension causes plan recompilation storms | Latency spikes | Rate-limit recompilation. Cache N most recent schemas. Log when schema changes. |
| Kubernetes API watcher uses too much memory | DaemonSet memory budget exceeded | Limit watch to current node's pods only (fieldSelector: spec.nodeName). |
| Typed column approach (duration_ms_int, _str) doubles column count | Memory overhead for sparse type variants | Null bitmaps are 1 bit/row. Sparse columns cost almost nothing. We measured 1.03x overhead. |

## Success Metrics

- Fast path (filter + forward): within 80% of current 4.7M lines/sec (>3.7M)
- SQL path (transform + OTLP out): >500K lines/sec (3.5x faster than vlagent)
- SQL path (transform + JSON out): >300K lines/sec (12x faster than Vector)
- Per-pipeline memory: <3 MB steady state
- 10 pipelines total memory: <50 MB
- Startup to first event processed: <100ms
- Plan recompilation: <5ms
- Config reload (SIGHUP): <50ms, no event loss
