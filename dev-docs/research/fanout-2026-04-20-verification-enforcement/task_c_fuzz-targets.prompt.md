# Workstream C: Fuzz targets for trust boundaries

You are adding fuzz targets for the 6 highest-priority uncovered trust boundaries.

## Objective

Implement fuzz targets for external trust boundary functions that currently have zero fuzzing coverage. Focus on the network-facing OTLP decoders and the disk-facing checkpoint segment recovery — the highest-risk gaps.

## Why this workstream exists

8 fuzz targets exist but 6 are scanner variants. The OTLP protobuf decoder accepts untrusted bytes over HTTP from any collector with zero fuzz coverage. A trust boundary audit found 16 uncovered functions across 4 crates.

Prior research (issue #2411 comment) found:
- 16 uncovered trust boundaries, 12 new fuzz targets designed
- Most I/O decode functions are `pub(super)` — need `#[cfg(fuzzing)]` re-exports
- Bolero recommended for new targets (CI-as-test via `cargo test`)
- Existing cargo-fuzz targets should stay as-is

## Mode

implementation

## Required execution checklist

- You MUST read all 8 existing fuzz targets in `crates/logfwd-core/fuzz/fuzz_targets/` for patterns
- You MUST implement these 6 fuzz targets (P0/P1 from the audit):
  1. `fuzz_otlp_protobuf_decode` — OTLP protobuf → RecordBatch (network input)
  2. `fuzz_otlp_json_decode` — OTLP JSON → RecordBatch (network input)
  3. `fuzz_decompress` — gzip/zstd decompression with expansion limits
  4. `fuzz_otap_decode` — OTAP protocol decoder
  5. `fuzz_arrow_ipc_decode` — Arrow IPC deserialization
  6. `fuzz_protojson_numbers` — protojson numeric parsing edge cases
- You MUST add `#[cfg(fuzzing)]` re-exports for any `pub(super)` functions that need fuzzing
- You MUST add corpus seeds for each target (valid protobuf, valid JSON, etc.)
- You MUST verify each target runs: `cargo +nightly fuzz run <target> -- -max_total_time=10`
- You MUST create `dev-docs/verification/fuzz-manifest.toml` listing all targets

After required work: explore bolero as an alternative to cargo-fuzz for unified harnesses.

## Required repo context

- `crates/logfwd-core/fuzz/` — existing targets and Cargo.toml
- `crates/logfwd-io/src/otlp_receiver/decode.rs` — OTLP protobuf decoder
- `crates/logfwd-io/src/otlp_receiver/convert.rs` — protobuf → Arrow
- `crates/logfwd-io/src/compress.rs` — decompression
- `crates/logfwd-io/src/otap_receiver.rs` — OTAP decoder
- `crates/logfwd-io/src/arrow_ipc_receiver.rs` — Arrow IPC

## Deliverable

- 6 new fuzz targets in `crates/logfwd-core/fuzz/fuzz_targets/` (or `crates/logfwd-io/fuzz/`)
- `dev-docs/verification/fuzz-manifest.toml`
- Any `#[cfg(fuzzing)]` re-exports needed
- Write findings to `dev-docs/research/fanout-2026-04-20-verification-enforcement/task_c_results.md`

## Constraints

- Fuzz targets must compile and run with `cargo +nightly fuzz`
- Each target must check: no panic, bounded allocation (no OOM on 1MB input)
- Corpus seeds must be valid protocol messages (not random bytes)
- Do NOT modify production code beyond adding `#[cfg(fuzzing)]` re-exports

## Success criteria

- All 6 targets run without crashing on seed corpus
- At least one target finds a new edge case during a 60-second run
- fuzz-manifest.toml is complete and parseable
