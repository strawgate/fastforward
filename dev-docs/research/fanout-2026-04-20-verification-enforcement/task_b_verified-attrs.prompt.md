# Workstream B: Compile-time verification attribute macros

You are building the `#[verified(kani = "...")]` attribute macro that provides compile-time enforcement of proof linkage.

## Objective

Create a proc-macro crate that provides `#[verified(kani = "proof_name")]` — when applied to a function, it generates a `#[cfg(kani)] const` assertion that the proof function exists. Missing proofs cause compile errors, not CI-time warnings.

## Why this workstream exists

Even with a CI-time structural checker (xtask verify), there's a 10-minute feedback loop. Compile-time enforcement catches missing proofs immediately during `cargo build`. A prototype confirmed the mechanism works: the `const` reference trick produces clear error messages.

Prior research (issue #2410 comment) found:
- 67 functions in logfwd-core need annotation (61 pub fn + 6 pub(crate) fn)
- Zero runtime cost confirmed via doc-marker pattern
- Opt-in `#[verified]` on proven functions is recommended over opt-out
- Extends the planned `logfwd-lint-attrs` crate pattern
- Kani contracts are complementary (specify WHAT vs ensure THAT)

## Mode

implementation

## Required execution checklist

- You MUST read how `#[cfg(kani)]` verification modules are structured (read 3-4 examples in `crates/logfwd-core/src/`)
- You MUST create or extend `crates/logfwd-lint-attrs/` as a proc-macro crate
- You MUST implement these attributes:
  1. `#[verified(kani = "verify_fn_name")]` — generates const reference to proof fn
  2. `#[trust_boundary]` — marks function as handling untrusted input (doc marker for xtask)
  3. `#[allow_unproven]` — explicit exemption from proof requirement (doc marker)
- You MUST annotate at least 20 functions in `crates/logfwd-core/src/otlp.rs` as proof of concept
- You MUST verify that `cargo build` fails when a proof is missing
- You MUST verify that `cargo build` succeeds when all proofs exist
- You MUST verify zero runtime cost (compare codegen with and without attributes)
- You MUST verify the error message is clear and actionable

After required work: explore whether `#[verified(proptest = "test_name")]` linkage is also feasible.

## Required repo context

- `crates/logfwd-core/src/otlp.rs` — proof module structure to link against
- `crates/logfwd-core/src/structural.rs` — another example
- `crates/logfwd-core/src/cri.rs` — another example
- `Cargo.toml` — workspace structure

## Deliverable

- `crates/logfwd-lint-attrs/Cargo.toml` + `crates/logfwd-lint-attrs/src/lib.rs`
- Annotated functions in `crates/logfwd-core/src/otlp.rs` (at least 20)
- Write findings to `dev-docs/research/fanout-2026-04-20-verification-enforcement/task_b_results.md`

## Constraints

- Proc-macro must work with stable Rust (no nightly required)
- Zero runtime cost (doc markers only)
- Must not break `cargo kani` or `cargo test`
- Error messages must point at the annotation site with the missing proof name

## Success criteria

- `cargo build` fails with clear error when proof is removed
- `cargo build` succeeds with all proofs present
- 20+ functions annotated in otlp.rs
- Zero runtime overhead confirmed
