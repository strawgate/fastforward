# logfwd Architecture

## Overview

logfwd is a high-performance log forwarder. It reads logs from multiple sources,
transforms them with SQL (via Apache DataFusion), and ships them to multiple
destinations. Everything goes through Arrow RecordBatches — the universal
internal representation.

## Pipeline Model

```
  Inputs              Transform              Outputs
  ──────              ─────────              ───────
  File (CRI)  ─┐                         ┌─  OTLP (HTTP)
  File (JSON)  │   Arrow RecordBatch      │   JSON lines (HTTP)
               ├──────────────────────►──┤   Stdout
               │        │                 │
               │   DataFusion SQL         │
               │   (always runs)          │
               │                          │   (Placeholders, not yet wired:)
               │                          │   Elasticsearch
               │                          │   Loki
               └─                         └─  Parquet / File
```

Every input produces rows. Every output consumes rows. The transform is SQL.
There is no "fast path" — DataFusion always runs, even for `SELECT * FROM logs`
(~85ns/line overhead, negligible).

## Internal Representation

Every batch is an Arrow RecordBatch with typed columns:

```
  _raw: Utf8                    — original JSON line bytes
  level_str: Utf8               — always string
  status_int: Int64             — JSON integer values
  status_str: Utf8              — JSON string values (when type conflicts)
  duration_ms_float: Float64    — JSON float values
  message_str: Utf8             — always string
  ...
```

**Naming convention:** `{field_name}_{type_suffix}` where suffix is `str`, `int`, or `float`.

**Type handling:** Fields with consistent types get one column. Fields with type
conflicts (e.g., `status` is sometimes `500`, sometimes `"healthy"`) get multiple
columns with NULLs in non-matching rows. Memory overhead: 1.03x (measured).

## Data Flow

```
1. Input reads raw bytes (file via tail.rs)
2. FormatParser (format.rs) converts to newline-delimited JSON
   - CRI parser strips container log envelope (if K8s container logs)
   - JSON parser passes through, handling partial lines
   - Raw parser wraps lines as JSON
3. Scanner (scanner.rs) parses JSON, writes into Arrow column builders
4. BatchBuilder (batch_builder.rs) produces RecordBatch with typed columns
5. DataFusion (logfwd-transform) executes cached SQL plan on RecordBatch
6. Output sinks (logfwd-output) serialize RecordBatch to OTLP/JSON/stdout
7. Network send / stdout write
```

The read buffer is released after step 4 — Arrow builders own their data.
DataFusion and outputs never touch the read buffer.

## Configuration (YAML)

Simple (one pipeline):
```yaml
input:
  type: file
  path: /var/log/pods/**/*.log
  format: cri

transform: |
  SELECT * EXCEPT (stack_trace)
  FROM logs
  WHERE level != 'DEBUG'

output:
  type: otlp
  endpoint: otel-collector:4317
  compression: zstd

server:
  diagnostics: 0.0.0.0:9090
```

Advanced (multiple named pipelines):
```yaml
pipelines:
  app_logs:
    inputs:
      - name: pod_logs
        type: file
        path: /var/log/pods/**/*.log
        format: cri
    transform: |
      SELECT * EXCEPT (stack_trace) FROM logs WHERE level != 'DEBUG'
    outputs:
      - name: collector
        type: otlp
        endpoint: otel-collector:4317
      - name: debug
        type: stdout
        format: json

server:
  diagnostics: 0.0.0.0:9090
```

Input types: `file`, `udp`, `tcp`, `otlp` (only `file` is implemented)
Output types: `otlp`, `http`, `stdout` (implemented); `elasticsearch`, `loki`, `file_out`, `parquet` (placeholder)
Formats: `cri`, `json`, `logfmt`, `syslog`, `raw`, `auto` (config accepts all; `cri`, `json`, `raw` have parsers)

## SQL Transform

Users write SQL. The system auto-detects field types and maps column names.

```sql
-- Simple filter
SELECT * FROM logs WHERE level_str != 'DEBUG'

-- Add computed fields
SELECT *, 'production' AS environment FROM logs
```

Built-in UDFs:
- `int(col)` — safe cast from Utf8 to Int64, NULL on failure
- `float(col)` — safe cast from Utf8 to Float64, NULL on failure
- `regexp_extract(col, pattern, group)` — regex capture group extraction
- `grok(col, pattern)` — Grok pattern matching

**Note:** The SQL rewriter (translating bare column names like `level` to
internal typed names like `level_str`) is not yet built. Currently, SQL
must reference the internal `{field}_{type}` column names directly.

## Diagnostics Server

HTTP server on configurable port. Runs on a dedicated thread, reads atomic
counters — no locking on the hot path.

| Endpoint | Content |
|----------|---------|
| `GET /` | Embedded HTML dashboard with auto-refreshing pipeline visualization |
| `GET /health` | `{"status":"ok","uptime_seconds":N,"version":"0.2.0"}` |
| `GET /metrics` | Prometheus text format (counters per input/output) |
| `GET /api/pipelines` | JSON with live pipeline stats (lines/sec, errors, etc.) |

## Threading Model

```
Main thread: parse config, build pipelines, start diagnostics
Per pipeline:
  Pipeline thread: poll inputs → format parse → scan → DataFusion → output sinks
Diagnostics thread: tiny_http server, reads atomic counters
OTel thread (optional): tokio runtime for periodic OTLP metric export
```

Each pipeline runs single-threaded. Multiple pipelines run on separate threads.
The diagnostics server runs on its own thread. If OTel metrics push is configured,
a small tokio runtime is created for the periodic exporter.

## Performance

Measured on Apple M-series (arm64), single core:

| Stage | Time/line | Source |
|-------|-----------|--------|
| File read (1MB buffer, page cache) | 17ns | v1 e2e bench |
| CRI parse + buffer copy | 53ns | v1 e2e bench |
| JSON scan all fields → Arrow | 399ns | Rust scan_bench |
| Arrow column construction only | 93ns | Rust scan_bench |
| DataFusion simple filter | 85ns | Python prototype |
| DataFusion complex transform | 505ns | Python prototype |
| OTLP protobuf encode (from bytes) | 100ns | v1 criterion bench |
| zstd-1 compression | 248ns | v1 e2e bench |

K8s benchmark (VictoriaMetrics log-collectors-benchmark, 0.25 cores):
- logfwd saturated at 108K lines/sec (= 432K/core)
- vlagent saturated at 65K lines/sec (= 260K/core)
- logfwd 1.7x faster at same CPU

## Workspace Structure

```
crates/
  logfwd/               Binary crate: CLI + pipeline wiring
  logfwd-core/          Core: scanner, batch_builder, otlp, cri, compress,
                        tail, diagnostics, format, input
  logfwd-config/        YAML config parser
  logfwd-transform/     DataFusion SQL engine + UDFs (int, float,
                        regexp_extract, grok)
  logfwd-output/        OutputSink trait + sinks (stdout, OTLP, JSON lines,
                        fanout; elasticsearch/loki/parquet placeholders)
  logfwd-bench/         Criterion benchmarks + report generator
```
