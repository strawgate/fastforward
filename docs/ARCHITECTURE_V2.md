# logfwd v2 Architecture

## Pipeline Model

Everything is a pipeline. A pipeline has inputs, a transform, and outputs.
The transform operates on a table — a batch of rows with named, typed columns.

```
                    ┌─────────────────────────────────────┐
                    │            Pipeline                  │
                    │                                      │
  ┌──────────┐     │  ┌─────────┐   ┌─────────────────┐  │     ┌──────────┐
  │  Input 1 │────▶│  │         │   │    Transform     │  │────▶│ Output 1 │
  └──────────┘     │  │  Table  │──▶│  (SQL / fast)    │──│     └──────────┘
  ┌──────────┐     │  │         │   │                  │  │     ┌──────────┐
  │  Input 2 │────▶│  │ (Arrow  │   │  SELECT ...      │  │────▶│ Output 2 │
  └──────────┘     │  │  Record │   │  FROM logs       │  │     └──────────┘
                    │  │  Batch) │   │  WHERE ...       │  │
                    │  └─────────┘   └─────────────────┘  │
                    └─────────────────────────────────────┘
```

The table is the universal interface. Every input produces rows into it.
Every output consumes rows from it. The transform is SQL.

## Configuration (YAML)

```yaml
# logfwd.yaml

# Simple case — one input, one output, optional transform
input:
  type: file
  path: /var/log/pods/**/*.log
  format: cri

transform: |
  SELECT * EXCEPT (stack_trace, debug_context)
  FROM logs
  WHERE level != 'DEBUG'

output:
  type: otlp
  endpoint: otel-collector:4317
  compression: zstd

# --- OR --- Advanced: named pipelines with routing

pipelines:
  app_logs:
    inputs:
      - name: pod_logs
        type: file
        path: /var/log/pods/**/*.log
        format: cri

      - name: syslog_in
        type: udp
        listen: 0.0.0.0:514
        format: syslog

    transform: |
      SELECT * EXCEPT (stack_trace)
             REPLACE (redact(message, 'email') AS message),
             'production' AS environment
      FROM logs
      WHERE level != 'DEBUG'

    outputs:
      - name: collector
        type: otlp
        endpoint: otel-collector:4317
        protocol: grpc
        compression: zstd

      - name: archive
        type: elasticsearch
        urls:
          - https://es-1:9200
          - https://es-2:9200
        index: logs-{service}-{date}
        compression: gzip

      - name: debug
        type: stdout
        format: json

  # Second pipeline — different input, different transform
  security_audit:
    inputs:
      - name: audit
        type: file
        path: /var/log/audit/*.log
        format: json

    transform: |
      SELECT * FROM logs
      WHERE action IN ('login_failed', 'permission_denied', 'key_rotated')

    outputs:
      - name: siem
        type: otlp
        endpoint: siem-collector:4317

# Global settings
server:
  diagnostics: 0.0.0.0:9090    # pipeline stats, health, Prometheus metrics
  log_level: info

storage:
  data_dir: /var/lib/logfwd     # checkpoints, WAL
```

## YAML Schema

### Input Types

```yaml
# File tailing
- type: file
  path: /var/log/**/*.log       # glob pattern, required
  format: cri | json | logfmt | syslog | raw | auto  # default: auto
  start_from: end | beginning   # default: end
  multiline:                    # optional
    pattern: "^\\d{4}-"         # regex for start of new entry

# UDP receiver
- type: udp
  listen: 0.0.0.0:514          # required
  format: syslog | json | raw  # default: syslog

# TCP receiver
- type: tcp
  listen: 0.0.0.0:5140         # required
  format: syslog | json | raw  # default: raw
  framing: newline | octet_counting  # default: newline

# OTLP receiver (gRPC + HTTP)
- type: otlp
  grpc_listen: 0.0.0.0:4317    # optional
  http_listen: 0.0.0.0:4318    # optional
```

### Output Types

```yaml
# OTLP (gRPC or HTTP)
- type: otlp
  endpoint: host:port           # required
  protocol: grpc | http         # default: grpc
  compression: zstd | gzip | none  # default: zstd
  resource:                     # static resource attributes
    service.name: myapp
    k8s.cluster.name: prod

# Elasticsearch
- type: elasticsearch
  urls: [https://es:9200]       # required
  index: logs-{date}            # supports {field} interpolation
  compression: gzip | none      # default: gzip
  auth:
    username: elastic
    password_env: ES_PASSWORD   # read from environment variable

# Loki
- type: loki
  endpoint: http://loki:3100
  labels:                       # static labels
    job: logfwd
  label_fields: [service, level]  # dynamic labels from log fields

# JSON lines over HTTP (VictoriaLogs, custom endpoints)
- type: http
  url: http://host:8080/insert/jsonline
  compression: zstd | gzip | none
  headers:
    Authorization: "Bearer ${TOKEN}"

# Local file
- type: file_out
  path: /var/log/forwarded/{date}.log
  rotation: daily | size:100MB
  compression: zstd | gzip | none

# Stdout (debugging)
- type: stdout
  format: json | text           # default: json

# S3 / GCS (future)
- type: s3
  bucket: my-log-archive
  prefix: logs/{date}/
  format: parquet | json
  batch_size: 100MB
```

### Transform

The transform is a SQL string. Applied to all rows from all inputs in the pipeline.

The implicit table name is `logs`. Every row has:
- All JSON fields as columns (auto-detected from data)
- `_raw`: the original log line as a string
- `_timestamp`: parsed or observed timestamp
- `_source`: which input produced this row

```yaml
transform: |
  SELECT * FROM logs WHERE level != 'DEBUG'
```

If no transform is specified, all rows pass through unchanged (fast path).

## Component Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Process                                  │
│                                                                  │
│  ┌─── Pipeline "app_logs" ─────────────────────────────────────┐│
│  │                                                              ││
│  │  Inputs:                                                     ││
│  │    FileInput(cri) ──┐                                        ││
│  │    UdpInput(syslog) ─┤                                       ││
│  │                      ▼                                       ││
│  │              ┌──────────────┐                                ││
│  │              │   Scanner    │ JSON parse → (offset,len,type) ││
│  │              │              │ per field per line              ││
│  │              └──────┬───────┘                                ││
│  │                     │                                        ││
│  │           ┌─────────┴──────────┐                             ││
│  │           │                    │                              ││
│  │      Fast path            SQL path                           ││
│  │   (simple filter)    (complex transform)                     ││
│  │           │                    │                              ││
│  │   Inline predicate    Arrow RecordBatch                      ││
│  │   evaluation          → DataFusion                           ││
│  │           │                    │                              ││
│  │           └─────────┬──────────┘                             ││
│  │                     │                                        ││
│  │              ┌──────▼───────┐                                ││
│  │              │   Fan-out    │                                 ││
│  │              └──┬───┬───┬──┘                                 ││
│  │                 │   │   │                                    ││
│  │  Outputs:       ▼   ▼   ▼                                   ││
│  │    OtlpSink  EsSink  StdoutSink                              ││
│  │                                                              ││
│  └──────────────────────────────────────────────────────────────┘│
│                                                                  │
│  ┌─── Pipeline "security_audit" ───────────────────────────────┐│
│  │  (same structure, independent buffers)                       ││
│  └──────────────────────────────────────────────────────────────┘│
│                                                                  │
│  ┌─── Diagnostics Server (:9090) ──────────────────────────────┐│
│  │  GET /health          → 200 OK                               ││
│  │  GET /metrics         → Prometheus format                    ││
│  │  GET /api/pipelines   → JSON: live pipeline stats            ││
│  │  GET /                → HTML: pipeline visualizer            ││
│  └──────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
```

## Diagnostics Server

A lightweight HTTP server (tiny_http or built-in — no async runtime needed)
on a configurable port. Three endpoints:

### GET /health

```json
{"status": "ok", "uptime_seconds": 3600, "version": "0.2.0"}
```

### GET /metrics (Prometheus)

```
# HELP logfwd_input_lines_total Total lines read per input
# TYPE logfwd_input_lines_total counter
logfwd_input_lines_total{pipeline="app_logs",input="pod_logs"} 48293741
logfwd_input_lines_total{pipeline="app_logs",input="syslog_in"} 1293847

# HELP logfwd_input_bytes_total Total bytes read per input
# TYPE logfwd_input_bytes_total counter
logfwd_input_bytes_total{pipeline="app_logs",input="pod_logs"} 10293847123

# HELP logfwd_output_lines_total Total lines sent per output
# TYPE logfwd_output_lines_total counter
logfwd_output_lines_total{pipeline="app_logs",output="collector"} 42938471
logfwd_output_lines_total{pipeline="app_logs",output="archive"} 42938471

# HELP logfwd_output_bytes_total Total bytes sent per output
# TYPE logfwd_output_bytes_total counter
logfwd_output_bytes_total{pipeline="app_logs",output="collector"} 1293847

# HELP logfwd_output_errors_total Send errors per output
# TYPE logfwd_output_errors_total counter
logfwd_output_errors_total{pipeline="app_logs",output="archive"} 3

# HELP logfwd_transform_lines_in Lines entering transform
# TYPE logfwd_transform_lines_in counter
logfwd_transform_lines_in{pipeline="app_logs"} 48293741

# HELP logfwd_transform_lines_out Lines exiting transform (after filter)
# TYPE logfwd_transform_lines_out counter
logfwd_transform_lines_out{pipeline="app_logs"} 42938471

# HELP logfwd_transform_filter_rate Percentage of lines dropped by filter
# TYPE logfwd_transform_filter_rate gauge
logfwd_transform_filter_rate{pipeline="app_logs"} 0.111

# HELP logfwd_pipeline_latency_seconds End-to-end latency (read to send)
# TYPE logfwd_pipeline_latency_seconds histogram
logfwd_pipeline_latency_seconds_bucket{pipeline="app_logs",le="0.01"} 39000000
logfwd_pipeline_latency_seconds_bucket{pipeline="app_logs",le="0.05"} 42000000
logfwd_pipeline_latency_seconds_bucket{pipeline="app_logs",le="0.1"} 42900000

# HELP logfwd_transform_path Which execution path is active
# TYPE logfwd_transform_path gauge
logfwd_transform_path{pipeline="app_logs",path="fast"} 1
logfwd_transform_path{pipeline="app_logs",path="sql"} 0

# HELP logfwd_files_watched Number of files currently tailed
# TYPE logfwd_files_watched gauge
logfwd_files_watched{pipeline="app_logs",input="pod_logs"} 47

# HELP logfwd_backpressure_stalls_total Times reader blocked on full channel
# TYPE logfwd_backpressure_stalls_total counter
logfwd_backpressure_stalls_total{pipeline="app_logs"} 0

# HELP logfwd_checkpoint_offset Current checkpoint offset per file
# TYPE logfwd_checkpoint_offset gauge
logfwd_checkpoint_offset{pipeline="app_logs",file="/var/log/pods/default_app-xyz/app/0.log"} 483927
```

### GET /api/pipelines (JSON — for UI)

```json
{
  "pipelines": [
    {
      "name": "app_logs",
      "transform": {
        "sql": "SELECT * EXCEPT (stack_trace) FROM logs WHERE level != 'DEBUG'",
        "path": "fast",
        "schema_fields": 23,
        "filter_drop_rate": 0.111
      },
      "inputs": [
        {
          "name": "pod_logs",
          "type": "file",
          "status": "running",
          "files_watched": 47,
          "lines_total": 48293741,
          "bytes_total": 10293847123,
          "lines_per_sec": 142000,
          "bytes_per_sec": 31200000,
          "errors": 0
        },
        {
          "name": "syslog_in",
          "type": "udp",
          "status": "running",
          "lines_total": 1293847,
          "lines_per_sec": 430,
          "errors": 0
        }
      ],
      "outputs": [
        {
          "name": "collector",
          "type": "otlp",
          "status": "connected",
          "lines_total": 42938471,
          "bytes_total": 1293847,
          "lines_per_sec": 126000,
          "compression_ratio": 13.2,
          "errors": 0,
          "backpressure_stalls": 0
        },
        {
          "name": "archive",
          "type": "elasticsearch",
          "status": "connected",
          "lines_total": 42938471,
          "bytes_total": 5293847000,
          "lines_per_sec": 126000,
          "errors": 3,
          "last_error": "timeout after 30s",
          "last_error_at": "2026-03-28T20:15:03Z"
        }
      ]
    }
  ],
  "system": {
    "uptime_seconds": 3600,
    "version": "0.2.0",
    "cpu_percent": 24.5,
    "memory_bytes": 115000000,
    "goroutines": 0,
    "threads": 8
  }
}
```

### GET / (HTML — pipeline visualizer)

A single-page HTML/JS dashboard that polls `/api/pipelines` every second
and renders a live DAG visualization. Each node shows:

- Component name and type
- Lines/sec (in and out)
- Error count (red badge if > 0)
- Backpressure indicator (yellow if stalling)

This is a static HTML file embedded in the binary (include_bytes!).
No build step, no npm, no framework. Just inline SVG or canvas rendering
with fetch() polling. ~500 lines of HTML/JS.

```
┌─────────────────────────────────────────────────────────┐
│  logfwd pipeline: app_logs                              │
│                                                          │
│  ┌──────────┐     ┌───────────┐     ┌──────────────┐   │
│  │ pod_logs  │     │ Transform │     │  collector    │   │
│  │ (file)    │────▶│ (fast)    │────▶│  (otlp)      │   │
│  │ 142K/s    │     │ -11% filt │     │  126K/s      │   │
│  │ 47 files  │     │ 23 fields │     │  13.2x ratio │   │
│  └──────────┘     └───────────┘     └──────────────┘   │
│  ┌──────────┐           │           ┌──────────────┐   │
│  │ syslog   │           │           │  archive      │   │
│  │ (udp)    │───────────┘           │  (es)         │   │
│  │ 430/s    │                  ────▶│  126K/s       │   │
│  └──────────┘                       │  3 errors ⚠   │   │
│                                      └──────────────┘   │
└─────────────────────────────────────────────────────────┘
```

## Internal Component Interfaces

### Input → Scanner

```rust
/// Every input produces MessageBatches.
trait Input: Send {
    /// Poll for new data. Non-blocking — returns empty batch if no data.
    fn poll(&mut self) -> io::Result<Option<MessageBatch>>;
    /// Input name (from config).
    fn name(&self) -> &str;
    /// Current stats.
    fn stats(&self) -> InputStats;
}

struct MessageBatch {
    /// Raw bytes — newline-delimited messages.
    buf: Vec<u8>,
    /// Source metadata (which input, observed timestamp).
    source: SourceMeta,
}
```

### Scanner → Transform

```rust
/// The scanner produces field positions from raw bytes.
/// Output depends on which path the transform needs.
enum ScanResult {
    /// Fast path: field refs for inline predicate + direct encoding.
    FastPath {
        lines: Vec<LineRef>,          // (offset, len) of each line in buf
        field_refs: Vec<FieldRefs>,   // per-line extracted fields
        passed: BitVec,               // which lines passed the predicate
    },
    /// SQL path: Arrow RecordBatch with typed columns.
    SqlPath {
        batch: RecordBatch,           // typed columns built from scan
        raw_lines: StringArray,       // _raw column
    },
}
```

### Transform → Output

```rust
/// What outputs receive after the transform.
trait Output: Send {
    fn send(&mut self, batch: &OutputBatch) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
    fn name(&self) -> &str;
    fn stats(&self) -> OutputStats;
}

enum OutputBatch<'a> {
    /// Fast path result.
    Lines {
        buf: &'a [u8],
        line_offsets: &'a [(u32, u32)],
        field_refs: &'a [FieldRefs],
    },
    /// SQL path result.
    Transformed {
        batch: &'a RecordBatch,
        typed_index: &'a TypedColumnIndex,
    },
}
```

### Stats (shared across all components)

```rust
/// Atomic counters, lock-free, read by diagnostics server.
struct ComponentStats {
    lines_total: AtomicU64,
    bytes_total: AtomicU64,
    errors_total: AtomicU64,
    lines_per_sec: AtomicU64,      // computed from rolling window
    last_error: Mutex<Option<String>>,
    last_error_at: AtomicU64,      // unix timestamp
}
```

Each component (input, transform, output) holds a `ComponentStats`.
The diagnostics server reads them without locking (atomics are Relaxed
ordering — we don't need precision, just approximate live values).

## Threading Model

```
Main thread:
  - Parse config
  - Start diagnostics server (spawns 1 thread)
  - For each pipeline:
    - Start reader thread (polls inputs, runs scanner)
    - Start N sender threads per output (serialize + send)
  - Wait for shutdown signal

Per pipeline:
  Reader thread:
    loop:
      for each input: poll() → MessageBatch
      scan batch → ScanResult
      transform (fast path or SQL)
      fan-out to output channels

  Sender threads (per output, configurable count):
    loop:
      receive OutputBatch from channel
      serialize (OTLP protobuf, ES bulk, JSON, etc.)
      send over network
      ack back to reader for checkpoint advance

Diagnostics thread:
  tiny_http server
  reads ComponentStats atomics
  serves /health, /metrics, /api/pipelines, /
```

## Build Order

### Phase 0: Config + Pipeline Shell (now)
- YAML config parser (serde_yaml)
- Pipeline struct with Input/Output trait stubs
- Wire existing daemon code into the new structure
- Diagnostics server with /health and /metrics

### Phase 1: Configurable Scanner + Fast Path Transform
- FieldExtractor from SQL column refs
- Inline predicate evaluation
- SELECT * EXCEPT in fast path

### Phase 2: Output Abstraction
- OutputSink trait implementations (OTLP, HTTP/jsonlines, stdout)
- Fan-out to multiple outputs
- Per-output channels + sender threads

### Phase 3: SQL Transform (DataFusion)
- Arrow RecordBatch construction from scanner
- DataFusion integration
- SQL rewriter
- Schema inference + caching

### Phase 4: Additional Inputs/Outputs
- UDP syslog input
- TCP input
- Elasticsearch output
- Loki output
- File output

### Phase 5: Durability
- WAL per output
- Checkpoint persistence
- End-to-end acks
- Graceful shutdown
