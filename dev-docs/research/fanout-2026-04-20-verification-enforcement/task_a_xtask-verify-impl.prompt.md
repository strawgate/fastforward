# Workstream A: Build `cargo xtask verify` structural checker

You are implementing the core verification enforcement tool for this repository.

## Objective

Build a `cargo xtask verify` command — a Rust binary that uses `syn` to parse all source files and enforces 6 structural verification constraints. This replaces manual documentation (VERIFICATION.md) with machine-enforced checks that fail CI.

## Why this workstream exists

An audit found VERIFICATION.md was wrong in 6 of 12 modules. `row_json.rs` has 484 lines and zero tests — nobody caught it. A prototype script confirmed that `syn`-based checking finds real gaps with low false-positive rates. The xtask pattern is proven (rustc's `tidy`, Servo's `tidy`).

Prior research (issue #2409 comment) found:
- Check 3 (pub_module_needs_tests): 8 true-positive zero-test modules, ~0% FP after exemptions
- Check 1 (pub_fn_needs_proof): 0 violations today (regression gate)
- Check 6 (proof_count_matches_manifest): 0 violations, mirrors existing Python script
- syn + raw text is recommended over tree-sitter (avoids C build dep)
- Estimated ~15h implementation for all 6 checks

## Mode

implementation

## Required execution checklist

- You MUST read `scripts/verify_kani_boundary_contract.py`, `scripts/verify_tlc_matrix_contract.py`, `dev-docs/verification/kani-boundary-contract.toml` before writing code.
- You MUST create the xtask binary at `xtask/src/main.rs` with a `verify` subcommand.
- You MUST implement these 6 checks:
  1. `pub_fn_needs_proof` — every pub fn in logfwd-core with non-trivial logic has a Kani proof
  2. `unsafe_needs_safety_comment` — every unsafe block has `// SAFETY:` comment
  3. `pub_module_needs_tests` — every module with pub fns has at least one test
  4. `encode_decode_roundtrip` — encode/decode pairs have roundtrip tests
  5. `trust_boundary_manifest` — functions in fuzz-manifest.toml have fuzz targets
  6. `proof_count_manifest` — Kani proof counts match kani-boundary-contract.toml
- You MUST add an exemption mechanism: `// xtask-verify: allow(check_name) reason: ...`
- You MUST add a `just verify` recipe that runs `cargo xtask verify`
- You MUST run the tool against the actual codebase and fix false positives
- You MUST add the tool to `.github/workflows/ci.yml`

After required work: explore whether checks can auto-fix violations (e.g., generating proof stubs).

## Required repo context

Read at least these:
- `scripts/verify_kani_boundary_contract.py` — existing enforcement to subsume
- `dev-docs/verification/kani-boundary-contract.toml` — seam manifest format
- `dev-docs/VERIFICATION.md` — what to replace
- `justfile` — existing recipes
- `.github/workflows/ci.yml` — CI integration
- `crates/logfwd-core/src/otlp.rs` — example of well-proven module
- `crates/logfwd-output/src/row_json.rs` — example of zero-test module

## Deliverable

- `xtask/Cargo.toml` + `xtask/src/main.rs` — the verification tool
- `justfile` update — `just verify` recipe
- `.github/workflows/ci.yml` update — CI integration
- Write findings to `dev-docs/research/fanout-2026-04-20-verification-enforcement/task_a_results.md`

## Constraints

- Use `syn` crate for parsing (not tree-sitter — avoids C build dep)
- Zero external tools required (just `cargo xtask verify`)
- Must complete in < 30 seconds on full codebase
- False positive rate must be < 5% after exemptions
- Do NOT delete VERIFICATION.md yet — mark it as auto-generated-from in a comment

## Success criteria

- Tool runs against codebase, finds the known gaps (row_json.rs zero tests, etc.)
- Zero false positives on modules that are properly verified
- CI integration works (recipe + workflow)
