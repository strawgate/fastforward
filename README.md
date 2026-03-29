# logfwd

A high-performance log forwarder written in Rust. Reads log files, transforms with SQL (via Apache DataFusion), and ships to OTLP, HTTP, or stdout. Everything goes through Arrow RecordBatches.

## Performance

Scanner + DataFusion component benchmarks (Apple M-series, single core):

| Stage | Lines/sec | Notes |
|-------|----------|-------|
| JSON scan → Arrow (13 fields) | 2.5M | full field extraction |
| JSON scan → Arrow (3 fields, pushdown) | 2.9M | field pushdown |
| DataFusion simple filter | ~12M | `WHERE level_str != 'DEBUG'` |
| OTLP protobuf encode (from bytes) | 10M | v1 criterion bench |

K8s benchmark (VictoriaMetrics log-collectors-benchmark, 0.25 cores):
- logfwd: 108K lines/sec (432K/core)
- vlagent: 65K lines/sec (260K/core)
- logfwd 1.7x faster at same CPU

## Quick Start

```bash
# Build
cargo build --release

# Generate test data
./target/release/logfwd --generate-json 5000000 /tmp/json_logs.txt

# Run from YAML config
./target/release/logfwd --config config.yaml

# Validate config without running
./target/release/logfwd --config config.yaml --validate

# Dry run: build pipelines, show schema, exit
./target/release/logfwd --config config.yaml --dry-run

# Start a blackhole OTLP collector (for benchmarks)
./target/release/logfwd --blackhole 127.0.0.1:4318
```

## Configuration

Simple (single pipeline):
```yaml
input:
  type: file
  path: /var/log/pods/**/*.log
  format: cri

transform: |
  SELECT * FROM logs WHERE level_str != 'DEBUG'

output:
  type: otlp
  endpoint: http://otel-collector:4318

server:
  diagnostics: 0.0.0.0:9090
```

Advanced (multiple pipelines):
```yaml
pipelines:
  app_logs:
    inputs:
      - name: pod_logs
        type: file
        path: /var/log/pods/**/*.log
        format: cri
    transform: |
      SELECT * FROM logs WHERE level_str != 'DEBUG'
    outputs:
      - name: collector
        type: otlp
        endpoint: http://otel-collector:4318
      - name: debug
        type: stdout
        format: json

server:
  diagnostics: 0.0.0.0:9090
```

## Output Types

| Type | Status | Description |
|------|--------|-------------|
| `otlp` | Implemented | OTLP protobuf over HTTP |
| `http` | Implemented | JSON lines over HTTP |
| `stdout` | Implemented | JSON or text to stdout |
| `elasticsearch` | Placeholder | Elasticsearch bulk API |
| `loki` | Placeholder | Loki push API |
| `parquet` | Placeholder | Parquet file output |

## Kubernetes Deployment

See `deploy/` for DaemonSet manifests. Compatible with the [VictoriaMetrics log-collectors-benchmark](https://github.com/VictoriaMetrics/log-collectors-benchmark) for head-to-head comparison.

```bash
# Build and load image into KIND
docker build -t logfwd:latest .
kind load docker-image --name log-collectors-bench logfwd:latest

# Deploy
kubectl apply -f deploy/daemonset.yml
```

## Architecture

See [DEVELOPING.md](DEVELOPING.md) for internal architecture and developer guide.
