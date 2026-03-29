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
  File (CRI)  ─┐                         ┌─  OTLP (gRPC/HTTP)
  File (JSON)  │   Arrow RecordBatch      │   JSON lines (HTTP)
  UDP syslog   ├──────────────────────►──┤   Elasticsearch
  TCP stream   │        │                 │   Stdout
  OTLP recv   ─┘   DataFusion SQL        └─  Parquet / File
                   (always runs)
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
1. Input reads raw bytes (file, UDP, TCP, OTLP)
2. CRI parser strips container log envelope (if K8s container logs)
3. Scanner (scanner.rs) parses JSON, writes into Arrow column builders
4. BatchBuilder (batch_builder.rs) produces RecordBatch with typed columns
5. SQL Rewriter translates user SQL to internal typed-column SQL [TODO]
6. DataFusion (transform.rs) executes cached SQL plan on RecordBatch
7. Output sinks (output.rs) serialize RecordBatch to OTLP/JSON/ES/Parquet
8. Network send / file write
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

Input types: `file`, `udp`, `tcp`, `otlp`
Output types: `otlp`, `http`, `elasticsearch`, `loki`, `stdout`, `file_out`, `parquet`
Formats: `cri`, `json`, `logfmt`, `syslog`, `raw`, `auto`

## SQL Transform

Users write SQL. The system auto-detects field types and maps column names.

```sql
-- Simple filter
SELECT * FROM logs WHERE level != 'DEBUG'

-- Drop and redact fields
SELECT * EXCEPT (stack_trace)
       REPLACE (redact(message, 'email') AS message)
FROM logs

-- Type-safe numeric operations
SELECT * FROM logs WHERE int(status) >= 400
SELECT AVG(float(duration_ms)) FROM logs GROUP BY service

-- Add computed fields
SELECT *, 'production' AS environment FROM logs
```

Built-in functions:
- `int(col)` — cast to integer, NULL on failure
- `float(col)` — cast to float, NULL on failure

The SQL rewriter (TODO) translates user column names to internal typed names:
- `level` → `level_str` (single type, no coalesce)
- `duration_ms` → `COALESCE(CAST(duration_ms_int AS VARCHAR), duration_ms_str)` (multi-type)
- `int(status)` → `COALESCE(status_int, TRY_CAST(status_str AS BIGINT))`

Schema is inferred from the first batch. Plan compiles once, recompiles on
schema changes (~1ms, rare — happens when new field names appear).

## Diagnostics Server

HTTP server on configurable port. Reads atomic counters — no locking on the hot path.

| Endpoint | Content |
|----------|---------|
| `GET /health` | `{"status":"ok","uptime_seconds":3600}` |
| `GET /metrics` | Prometheus text format (counters per input/output) |
| `GET /api/pipelines` | JSON with live pipeline stats (lines/sec, errors, etc.) |
| `GET /` | Embedded HTML dashboard with auto-refreshing pipeline visualization |

## Threading Model

```
Main thread: parse config, start pipelines
Per pipeline:
  Reader thread: poll inputs → CRI parse → scan → DataFusion → fan-out to channels
  Sender threads (per output): receive batch → serialize → send
Diagnostics thread: tiny_http server, reads atomic counters
```

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

## Codebase

```
src/
  scanner.rs        533  JSON→Arrow scanner with field pushdown
  batch_builder.rs  574  Typed column builders (str/int/float)
  transform.rs      691  DataFusion SQL engine, UDFs, plan caching
  output.rs         945  OutputSink: Stdout, JSON lines, OTLP, FanOut
  config.rs         633  YAML config parser
  diagnostics.rs    602  HTTP diagnostics server + dashboard
  otlp.rs           726  Hand-rolled OTLP protobuf encoder
  cri.rs            292  CRI container log parser
  chunk.rs          307  Double-buffer chunk accumulator
  compress.rs       195  zstd compression + wire format
  tail.rs           618  File tailer (notify + poll)
  daemon.rs         345  K8s DaemonSet mode (v1)
  pipeline.rs       337  v1 chunk pipeline
  e2e_bench.rs      241  v1 E2E benchmark harness
  tuner.rs          570  Adaptive chunk size tuner
  read_tuner.rs     211  Read buffer auto-tuner
  sender.rs         105  HTTP sender (ureq)
  main.rs           541  CLI entrypoint
  dashboard.html    138  Embedded pipeline visualizer
  ─────────────────────
  Total:          8,621 lines, 96 tests
```
