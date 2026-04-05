# 05. Executable Contract Suite Upgrade

This pass closes the biggest gap in the earlier POC: we now have more than a
single loopback test.

## What was added

- A written edge contract in [`dev-docs/ADAPTER_CONTRACT.md`](../../../dev-docs/ADAPTER_CONTRACT.md)
- OTLP receiver semantic contract tests covering:
  - JSON vs protobuf equivalence
  - zstd-compressed protobuf equivalence
  - typed attribute preservation (`int`, `double`, `bool`, `bytes`)
  - explicit rejection of malformed `bytesValue`
- A stronger OTLP sink -> receiver loopback test that now exercises:
  - zstd compression
  - typed attributes
  - resource attributes
  - trace/span IDs
- File boundary replay tests that assert:
  - checkpoints stop at real newline boundaries
  - multi-file partial lines remain isolated by `SourceId`
  - per-source checkpoints advance independently

## Why this is materially better

The earlier loopback test proved one happy path.
The new suite proves classes of behavior:

- format-path equivalence
- transport-path equivalence
- compression-path equivalence
- newline/checkpoint invariants
- per-source isolation invariants

That is much closer to a durable correctness harness than “one e2e test per bug”.

## Remaining work

- OTLP gRPC parity fixtures
- explicit copytruncate replay timelines
- a file collector baseline for external replay/durability differential tests
- benchmark guardrails for the contract suite overhead
