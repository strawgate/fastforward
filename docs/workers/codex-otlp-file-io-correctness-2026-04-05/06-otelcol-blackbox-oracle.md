# 06. Official Collector Black-Box Oracle

This pass adds external oracle tests instead of only comparing our code to our
code.

## What was added

- `crates/logfwd-io/tests/it/otelcol_blackbox_contract.rs`
- `crates/logfwd-io/tests/it/otlp_contract_support.rs`

The black-box contract file now contains two ignored external-oracle tests:

1. `our OtlpSink -> official otelcol receiver`
2. `official otelcol sender -> our OtlpReceiverInput`

Both tests:

1. Locate or download an official `otelcol-contrib` release binary.
2. Start a real Collector process with a file exporter for oracle capture.
3. Normalize the Collector's OTLP JSON into the same semantic row form used by
   the rest of the contract suite.
4. Compare boundary behavior at the semantic level instead of raw bytes.

## Why this matters

This is stronger than `our sink -> our receiver`:

- the receiver is now the official Collector, not our implementation
- the observed output comes from Collector state after real HTTP/protobuf decode
- failures catch protocol drift that internal loopback tests can hide

In other words, this is the first true external compatibility oracle coverage
in the OTLP contract suite.

## Why the test is ignored by default

It downloads and launches an external binary, so it is intentionally not part
of the always-on fast integration path.

The standard suite still verifies our internal contract layers quickly.
The ignored test is the heavier compatibility proof you run when you want
official-Collector confirmation.

## Current gap after this change

- We now have official Collector oracles for both **our OTLP output** and
  **our OTLP input**.
- We still need gzip parity and gRPC parity to match the OTLP ecosystem more
  completely.
- We also still need a comparable external oracle for file replay behavior,
  not just OTLP transport behavior.
