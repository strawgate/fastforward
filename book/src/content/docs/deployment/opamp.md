---
title: OpAMP Central Management
description: Manage ffwd instances remotely using the OpAMP protocol
---

ffwd supports [OpAMP](https://opentelemetry.io/docs/specs/opamp/) (Open Agent
Management Protocol) for centralized fleet management. An OpAMP server can:

- Push configuration changes to ffwd instances
- Monitor agent health and effective configuration
- Issue restart/reconfigure commands

## Quick start

Add an `opamp` section to your ffwd config:

```yaml
pipelines:
  default:
    inputs:
      - type: file
        path: /var/log/*.log
        format: json
    outputs:
      - type: otlp
        endpoint: http://collector:4317

opamp:
  endpoint: http://opamp-server:4320/v1/opamp
```

ffwd will connect to the OpAMP server and begin reporting its identity,
health, and effective configuration.

## Configuration reference

```yaml
opamp:
  # Required: OpAMP server endpoint (HTTP polling transport)
  endpoint: http://opamp-server:4320/v1/opamp

  # Optional: API key for authentication
  api_key: "your-api-key"

  # Instance UID: "auto" generates and persists a UUID (default),
  # or specify a fixed UUID
  instance_uid: auto

  # Service name reported to the server (default: "ffwd")
  service_name: ffwd

  # How often to poll the server in seconds (default: 30)
  poll_interval_secs: 30

  # Whether to accept remote config pushed by the server (default: true)
  accept_remote_config: true
```

## How it works

### Identity

Each ffwd instance gets a stable UUID. When `instance_uid: auto` (the default),
the UUID is generated on first run and persisted to `<data_dir>/opamp_instance_uid`.
This ensures the server sees the same agent across restarts.

### Remote configuration

When `accept_remote_config: true`, the OpAMP server can push new YAML
configuration to ffwd. The flow:

1. Server sends `AgentRemoteConfig` with a `config_map` entry
2. ffwd validates the YAML against its config schema
3. If valid: writes to `<data_dir>/opamp_remote_config.yaml` and triggers reload
4. If invalid: logs an error and rejects the config

This uses the same reload mechanism as SIGHUP and `--watch-config` — all
reload triggers feed the same channel.

### Effective configuration reporting

After each successful reload, ffwd reports its current effective configuration
back to the OpAMP server. The server can use this to verify that pushed configs
were actually applied.

### Health reporting

ffwd reports its health status to the OpAMP server on each poll cycle.

## Building with OpAMP support

OpAMP is an optional feature. To build ffwd with OpAMP:

```bash
cargo build --release -p ffwd --features opamp
```

Or to include it in the runtime:

```bash
cargo build --release -p ffwd-runtime --features opamp
```

## Compatible servers

ffwd's OpAMP client works with any server implementing the OpAMP protocol:

- [opamp-go reference server](https://github.com/open-telemetry/opamp-go) — official Go implementation
- [OpAMP Supervisor](https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/main/cmd/opampsupervisor) — OTel Collector's supervisor
- Any custom server implementing the [OpAMP spec](https://opentelemetry.io/docs/specs/opamp/)

### Testing with the Go reference server

```bash
# Clone and run the reference server
git clone https://github.com/open-telemetry/opamp-go
cd opamp-go/internal/examples/server
go run .
# Server starts on 0.0.0.0:4320

# Point ffwd at it
ff run --config ffwd.yaml
# (with opamp.endpoint: http://localhost:4320/v1/opamp in the config)
```

## Architecture

```text
┌─────────────┐         ┌──────────────┐         ┌─────────────┐
│ OpAMP Server│◄────────│  OpampClient │────────►│  bootstrap  │
│             │  HTTP    │ (background) │ reload  │ reload loop │
└─────────────┘  poll    └──────────────┘  tx     └─────────────┘
```

The OpAMP client runs as a background tokio task alongside the pipeline.
It shares the same reload channel as SIGHUP and file watching, so all
reload triggers are unified.

## Limitations

- Only HTTP polling transport is supported (not WebSocket — planned)
- Package management is not supported (agent updates via OpAMP)
- Connection settings offers are acknowledged but not applied
