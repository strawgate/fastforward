# Input Types

## File

Tail one or more log files with glob pattern support.

```yaml
input:
  type: file
  path: /var/log/pods/**/*.log
  format: cri      # cri | json | raw | auto
```

- **Glob re-scanning**: New files matching the pattern are discovered automatically (every 5s).
- **Rotation handling**: Detects file rotation (rename + create) and switches to the new file. Drains remaining data from the old file before switching.
- **Formats**: CRI (Kubernetes container runtime), JSON (newline-delimited), raw (plain text, each line becomes `{"_raw": "..."}`)

## Generator

Emit synthetic JSON log lines for benchmarking and pipeline testing. No external
data source is required.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `listen` | string | No | (unlimited) | Backward-compatible shorthand for target events per second. Prefer `generator.events_per_sec` for new configs. |
| `generator.events_per_sec` | integer | No | `0` | Target events per second. `0` means unlimited. |
| `generator.batch_size` | integer | No | `1000` | Events emitted per poll/batch. |
| `generator.total_events` | integer | No | `0` | Total events to emit before stopping. `0` means infinite. |
| `generator.profile` | string | No | `logs` | `logs` for generic synthetic request logs, `benchmark` for stable benchmark envelope rows. |
| `generator.complexity` | string | No | `simple` | Size/shape for the `logs` profile: `simple` or `complex`. Ignored by the `benchmark` profile. |
| `generator.benchmark_id` | string | No | unset | Included on every `benchmark` row. |
| `generator.pod_name` | string | No | input name | Source identity used by the `benchmark` profile. |
| `generator.stream_id` | string | No | `pod_name` | Stable stream identity used to build `event_id`. |
| `generator.service` | string | No | `bench-emitter` | Service name for the `benchmark` profile. |

```yaml
input:
  type: generator
  generator:
    events_per_sec: 50000
    batch_size: 4096
    profile: benchmark
    benchmark_id: ${BENCHMARK_ID}
    pod_name: ${POD_NAME}
    stream_id: ${POD_NAME}
```

Use `--generate-json <num_lines> <output_file>` on the CLI to write a fixed number of lines to a file instead.

## UDP

Receive log lines on a UDP socket.

```yaml
input:
  type: udp
  listen: 0.0.0.0:514
  format: json
```

## TCP

Accept log lines on a TCP socket.

```yaml
input:
  type: tcp
  listen: 0.0.0.0:5140
  format: json
```

## OTLP

Receive OTLP log records from another agent or SDK.

```yaml
input:
  type: otlp
```
