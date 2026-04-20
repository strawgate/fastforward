# Workstream D: Proptest coverage for zero-test output modules

You are adding proptest coverage to the output formatting modules that currently have zero or minimal tests.

## Objective

Add property-based tests to `row_json.rs` (zero tests), `json_lines.rs` (no proptest), and build a shared `arb_record_batch` generator for reuse across all output sink tests.

## Why this workstream exists

`row_json.rs` has 484 lines of production code and ZERO tests of any kind. It handles 18+ Arrow DataType variants for JSON serialization. It has two separate serialization paths (`write_row_json` and `write_row_json_resolved`) that should agree but are never tested together.

Prior research (issue #2413 comment) found:
- 18+ Arrow DataType match arms in row_json.rs
- Edge cases: NaN/Infinity → null, JSON string escaping of control chars, empty structs, nested structs, all-null rows
- Two serialization paths should be oracle-tested against each other
- `proptest-state-machine` is already a dependency (for framed.rs)
- No shared `arb_record_batch` generator exists — each sink has its own ad-hoc builder

## Mode

implementation

## Required execution checklist

- You MUST read `crates/logfwd-output/src/row_json.rs` completely before writing tests
- You MUST read `crates/logfwd-output/src/json_lines.rs` completely
- You MUST read existing proptest patterns in `crates/logfwd-output/src/elasticsearch.rs` and `crates/logfwd-output/src/otlp_sink.rs`
- You MUST implement a shared `arb_record_batch` generator in `crates/logfwd-test-utils/src/arrow.rs` (or similar) supporting: Utf8, Int64, Float64, Boolean, Null, Timestamp, List, Struct types
- You MUST add these proptests to `row_json.rs`:
  1. `row_json_produces_valid_json` — arbitrary batch → serialize → serde_json::from_str succeeds
  2. `row_json_field_count_matches_schema` — output JSON object has same field count as schema columns
  3. `row_json_null_handling` — null values serialize correctly (not missing, not "null" string)
  4. `row_json_paths_agree` — `write_row_json` and `write_row_json_resolved` produce identical output for same input
  5. `row_json_special_floats` — NaN and Infinity serialize as null
- You MUST add these proptests to `json_lines.rs`:
  1. `ndjson_lines_are_valid_json` — each line parses as valid JSON
  2. `ndjson_no_embedded_newlines` — no raw newlines inside JSON values
- You MUST verify all new tests pass: `cargo test -p logfwd-output`

After required work: explore adding `proptest-state-machine` tests for `framed.rs` event sequences.

## Required repo context

- `crates/logfwd-output/src/row_json.rs` — THE zero-test module
- `crates/logfwd-output/src/json_lines.rs` — minimal tests
- `crates/logfwd-output/src/elasticsearch.rs` — proptest oracle pattern to follow
- `crates/logfwd-test-utils/` — shared test utilities
- `crates/logfwd-output/Cargo.toml` — dependencies (check if proptest is already a dev-dep)

## Deliverable

- Shared `arb_record_batch` generator
- 5+ proptests in `row_json.rs`
- 2+ proptests in `json_lines.rs`
- Write findings to `dev-docs/research/fanout-2026-04-20-verification-enforcement/task_d_results.md`

## Constraints

- Tests must complete in < 30 seconds each (default proptest cases)
- Use existing proptest conventions from the repo
- Do NOT modify production code — only add tests
- The `arb_record_batch` generator must be reusable by other crates

## Success criteria

- `cargo test -p logfwd-output -- row_json` passes with 5+ new property tests
- `cargo test -p logfwd-output -- json_lines` passes with 2+ new property tests
- Shared generator works for at least 8 Arrow types
- No false property violations on valid inputs
