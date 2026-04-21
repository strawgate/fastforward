# Research: InputConfig V2 Tagged Enum Migration (Issue #1101)

> **Status:** Blocked (repository content mismatch)
> **Date:** 2026-04-21
- **Researcher:** Codex (ws03)

## Executive Summary

I attempted to perform the requested InputConfig V2 migration research for `logfwd`, but the required code paths and symbols are not present in this repository checkout.

Specifically, none of the following requested paths exist:

- `crates/logfwd-config/src/types.rs`
- `crates/logfwd-runtime/src/pipeline/`
- `crates/logfwd/src/commands.rs`
- `crates/logfwd/src/validate.rs`
- `crates/logfwd-config/src/validate.rs`

Likewise, the symbols `InputConfig` and `OutputConfigV2` do not appear in the codebase.

Because the target sources are unavailable, I cannot produce a factual inventory of fields/consumers or a safe migration plan tied to real call sites without fabricating details. This report documents what was verified and the exact unblock path.

---

## 1) Current InputConfig inventory: all fields, all consumers

## Result: **Not available in this checkout**

### Verification steps run

1. Searched for expected Rust config files and crates:
   - `rg --files | rg 'crates/logfwd|InputConfig|OutputConfigV2'`
   - No matches.

2. Searched recursively for symbols:
   - `rg -n "logfwd|InputConfig|OutputConfigV2|Issue #1101" .`
   - Found only metric-name string occurrences like `"logfwd.inflight_batches"`; no config type definitions.

3. Checked for Rust workspaces:
   - `rg --files -g 'Cargo.toml'`
   - Only:
     - `research/simd-experiments/Cargo.toml`
     - `packages/o11ytsdb/rust/Cargo.toml`

4. Searched the only substantial Rust subtree:
   - `rg -n "struct InputConfig|enum OutputConfigV2|InputConfig|OutputConfigV2" packages/o11ytsdb/rust`
   - No matches.

### Conclusion

The required `logfwd` crates are absent, so InputConfig field inventory and consumer inventory cannot be derived in this repository state.

---

## 2) Proposed InputConfigV2 enum design

## Result: **Design withheld to avoid speculative architecture drift**

Without the existing `InputConfig` field set and validation/runtime behavior, proposing a concrete enum shape would be speculative and potentially harmful.

That said, once sources are available, the recommended method is:

1. Enumerate existing flat fields and classify into:
   - transport-specific
   - parse/format-specific
   - shared metadata/control fields
2. Define tagged enum variants (`#[serde(tag = "type")]`) with per-variant structs.
3. Keep truly shared fields in wrapper or embedded common struct only if semantically universal.
4. Add conversion/compat layer for existing flat configs.

---

## 3) Migration strategy (big-bang vs incremental, compat parser)

## Result: **Pending source access**

Unable to decide safely without viewing current parser/loader/validation plumbing.

Default recommendation when unblocked:

- **Incremental migration with dual-format parse compatibility**:
  1. Introduce `InputConfigV2` tagged enum.
  2. Keep V1 flat parse support behind compatibility shim.
  3. Normalize into one internal representation after parse.
  4. Emit deprecation warnings for V1 flat input syntax.
  5. Remove V1 support in a later major/release gate.

---

## 4) Call site impact analysis with file paths

## Result: **Blocked**

Requested target paths do not exist in this checkout, so no call-site matrix can be built.

---

## 5) User config migration guide

## Result: **Template only (pending real schema)**

When unblocked, migration guide should include:

1. **Old flat input examples** for each input kind.
2. **Equivalent tagged enum examples** with `type` discriminator.
3. Mapping table: old fields → variant struct fields.
4. Backward-compat behavior and deprecation timeline.
5. Validation error examples and autofix hints.

---

## 6) Implementation plan: ordered steps

## Result: **Prepared as an unblock-ready checklist**

1. Locate current `InputConfig` and enumerate all optional fields/types.
2. Inventory all build + validate + CLI consumers.
3. Mirror `OutputConfigV2` compat approach for inputs.
4. Define `InputConfigV2` variants and shared/common fields policy.
5. Implement dual parser (flat V1 + tagged V2).
6. Convert runtime builders to exhaustive enum matching.
7. Update validators to variant-local rules.
8. Add fixture tests for V1 and V2 equivalence.
9. Add warnings/docs/changelog and deprecation policy.
10. Plan removal milestone for V1.

---

## 7) Risk assessment

## Current risk: **High uncertainty due to missing sources**

Primary risks once unblocked:

- Hidden coupling between flat options and runtime defaults.
- Backward-compat parsing edge cases in user configs.
- Validation divergence between CLI/runtime/config crates.
- Incomplete variant exhaustiveness during migration.

Mitigations:

- Golden config fixtures across all input kinds.
- Round-trip parse tests V1↔normalized↔V2.
- Exhaustive builder match + compile-time checks.
- Explicit deprecation logging and release notes.

---

## Unblock requirements

To complete the requested report with concrete field counts, variant definitions, and impacted files, provide one of:

1. A checkout/branch that includes the `crates/logfwd-*` sources listed in the request.
2. The specific files pasted or attached.
3. Corrected paths if these crates were moved/renamed.

Once available, I can deliver the full requested analysis in this same report path.
