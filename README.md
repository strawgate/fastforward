# logfwd

A high-performance log forwarder written in Rust. Reads log files, parses CRI container format, encodes to OTLP protobuf or JSON lines, compresses, and ships to a remote endpoint.

## Performance

Measured on Apple M-series (arm64), single core:

| Pipeline | Lines/sec | Notes |
|----------|----------|-------|
| CRI parse only | 26.4M | read + parse, no encoding |
| JSON lines + zstd | 6.0M | newline-delimited JSON, compressed |
| OTLP protobuf + zstd | 4.7M | full field extraction, compressed |
| OTLP raw body + zstd | 5.3M | body = raw line, no JSON parse |

Validated end-to-end in the [VictoriaMetrics log-collectors-benchmark](https://github.com/VictoriaMetrics/log-collectors-benchmark) Kubernetes setup at ~1M lines/sec with CRI parse, JSON field injection, and HTTP POST to the log-verifier, matching vlagent's throughput on the same hardware.

## Quick Start

```bash
# Build
cargo build --release

# Generate test data
./target/release/logfwd --generate-json 5000000 /tmp/json_logs.txt

# Benchmark (reads from file, no networking)
./target/release/logfwd /tmp/json_logs.txt --mode otlp

# End-to-end benchmark with CRI parsing
./target/release/logfwd --e2e --wrap-cri /tmp/json_logs.txt /tmp/cri_logs.txt
./target/release/logfwd --e2e /tmp/cri_logs.txt otlp-zstd

# Live tail a file
./target/release/logfwd /path/to/logfile --tail --mode otlp

# Run as Kubernetes DaemonSet daemon
./target/release/logfwd --daemon --glob "/var/log/containers/*.log" --endpoint "http://host:8080/insert/jsonline"
```

## Output Modes

| Mode | Description |
|------|-------------|
| `passthrough` | Read + count lines, no encoding or compression |
| `raw` | Compress raw newline-delimited bytes with zstd |
| `otlp` | Parse JSON fields, encode as OTLP protobuf, compress |

## E2E Benchmark Modes

| Mode | Description |
|------|-------------|
| `cri-only` | CRI parse only, discard output |
| `jsonlines` | CRI parse + JSON field injection |
| `jsonlines-zstd` | Above + zstd compression |
| `otlp` | CRI parse + OTLP protobuf (with JSON field extraction) |
| `otlp-raw` | CRI parse + OTLP protobuf (raw line as body) |
| `otlp-zstd` | OTLP with field extraction + zstd |
| `otlp-raw-zstd` | OTLP raw body + zstd |

## Kubernetes Deployment

See `deploy/` for DaemonSet manifests. Compatible with the [VictoriaMetrics log-collectors-benchmark](https://github.com/VictoriaMetrics/log-collectors-benchmark) for head-to-head comparison with vlagent, Vector, Fluent Bit, etc.

```bash
# Build and load image into KIND
docker build -t logfwd:latest .
kind load docker-image --name log-collectors-bench logfwd:latest

# Deploy
kubectl apply -f deploy/daemonset.yml
```

## Architecture

See [DEVELOPING.md](DEVELOPING.md) for internal architecture details.
