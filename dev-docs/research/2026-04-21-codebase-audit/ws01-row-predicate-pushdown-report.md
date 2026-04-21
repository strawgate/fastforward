# WS01 Research Report: Row Predicate Pushdown Design (Issue #1257)

> **Status:** Active research
> **Date:** 2026-04-21
> **Context:** Design-only proposal (no code changes)

## 0) Audit outcome and constraints

I attempted a direct code audit of the exact files listed in the issue prompt:

- `crates/logfwd-core/src/json_scanner.rs`
- `crates/logfwd-arrow/src/streaming_builder/mod.rs`
- `crates/logfwd-arrow/src/scanner.rs`
- `crates/logfwd-core/src/scan_config.rs`
- `crates/logfwd-transform/src/**`

In this checkout, those paths (and the `crates/logfwd-*` tree) are not present. I also searched the full `/workspace` tree for those filenames and found no matches. Because of that, this report is a **design proposal anchored to the pipeline described in the issue text**, not a line-by-line commentary of current implementation internals.

That said, the proposal below is structured so it can be directly mapped into the referenced files once they are available.

---

## 1) Current pipeline model (from issue context)

From the issue description, current behavior is:

1. Scanner reads line ranges in `scan_streaming()`.
2. Per-row field extraction occurs (with field-level pushdown via `ScanConfig::wanted_fields`).
3. `streaming_builder` receives row lifecycle callbacks (`begin_row()`, field appends, `end_row()`).
4. Batches are passed into SQL/DataFusion stage.
5. SQL `WHERE` filters execute after batch creation.

### Cost profile today

Even with field-level pushdown, **row-level costs still happen for rows later rejected by SQL**:

- JSON row parse/line decode (always)
- Per-row predicate-relevant field extraction (always)
- Arrow builder row accounting + null/default handling (always)
- Memory growth / validity bitmaps for filtered-out rows (always)

So for selective filters (e.g., 1–10% pass rate), most scan-stage CPU is spent building rows that are immediately discarded.

---

## 2) Predicate pushdown capability matrix

## 2.1 Pushdown-safe now (v1)

These are realistic and high-value to support first:

1. **String equality / inequality**
   - `field = 'literal'`, `field != 'literal'`
   - Examples: `level = 'error'`, `service = 'api'`
2. **Numeric comparisons**
   - `>`, `>=`, `<`, `<=`, `=`, `!=` on numeric fields
   - Example: `severity_number >= 17`
3. **Null checks**
   - `field IS NULL`, `field IS NOT NULL`
4. **Boolean composition**
   - `AND`, `OR`, `NOT` over pushdown-safe leaves

## 2.2 Conditionally pushable (v1.5)

- `IN (...)` for literal sets (can lower to OR chain)
- Simple casts if lossless and known (`Int64` vs `UInt64` compatibility)

## 2.3 Not pushdown-safe initially (non-goals for v1)

- `LIKE`, regex, string functions (`lower`, `substr`, etc.)
- Arithmetic expressions across fields (`a + b > 10`)
- UDFs / volatile functions (`now()`, random)
- Complex casts with ambiguity (`timestamp`, locale-sensitive parsing)
- Nested/array-heavy predicates unless scanner already extracts those cheaply

---

## 3) Where filtering should happen

## 3.1 Option analysis

### Option A — filter inside `json_scanner.rs` before full field extraction

**Pros**
- Maximum win: skip builder work and skip non-predicate field extraction.
- Preserves field-level pushdown synergy: only parse fields needed for predicate + projected fields.

**Cons**
- Requires predicate evaluator in scanner layer.
- Must carefully handle missing/type-mismatch semantics to match SQL three-valued logic.

### Option B — filter at `streaming_builder` row lifecycle

**Pros**
- Smaller integration surface if scanner already emits row-enter/row-exit events.

**Cons**
- Too late for best gains: many extraction costs already paid.
- Builder complexity rises (partial-row aborts, null bitmap consistency).

### Option C — filter batch after scan but before SQL transform

**Pros**
- Easiest to implement quickly using Arrow compute kernels.

**Cons**
- Lowest benefit: builder and scan costs already incurred.
- Mostly duplicates DataFusion filter stage.

## 3.2 Recommendation

**Primary:** Option A, with a narrow fallback to Option C for unsupported predicates.

Rationale:

- Option A is the only approach that removes the expensive “parse + append + allocate” path for dropped rows.
- Option C can still be useful as a compatibility bridge while predicate lowering coverage grows.

---

## 4) Proposed architecture

## 4.1 High-level flow

```text
SQL WHERE expr
   │
   ▼
DataFusion logical Expr
   │
   ▼
Pushdown planner (lower Expr -> ScanPredicateIR)
   │                  │
   │ success          │ partial/failed
   ▼                  ▼
ScanConfig.row_predicate = Some(IR)    ScanConfig.row_predicate = None
   │                                   + residual filter kept in DF plan
   ▼
json_scanner.scan_streaming()
   ├─ extract only predicate fields first
   ├─ eval IR with SQL 3VL semantics
   ├─ if false/null => skip row (no builder begin/end)
   └─ if true => extract projected fields + append row
```

## 4.2 Components

1. **`ScanPredicateIR` (new core type)**
   - Canonical pushdown expression tree:
     - `And(Vec<...>)`, `Or(Vec<...>)`, `Not(Box<...>)`
     - `Cmp { field, op, literal }`
     - `IsNull { field }`, `IsNotNull { field }`

2. **Predicate field dependency set**
   - Derived from IR (e.g., `{level, severity_number}`).
   - Used by scanner to parse minimal fields before deciding keep/drop.

3. **Tri-state evaluator**
   - Returns `{True, False, Unknown}` to preserve SQL null semantics.
   - Row admitted only when result is `True`.

4. **Residual predicate handling**
   - If only part of SQL predicate lowers to IR, keep remainder in DataFusion plan.
   - Guarantees correctness while still reducing row volume.

---

## 5) DataFusion integration strategy

## 5.1 Can DataFusion push predicates to a custom `TableProvider`?

Yes, by implementing filter pushdown signaling in the provider/scan planning interface and translating supported `Expr` nodes into source-native predicate IR. The exact trait/method names depend on the DataFusion version in use, but the pattern is stable:

1. Planner hands candidate filter expressions to table source.
2. Source reports which filters are supported/exact/inexact.
3. Supported filters are applied at scan.
4. Residual filters remain in execution plan.

## 5.2 Practical plan for this codebase

- Add a predicate-lowering layer in/near `crates/logfwd-transform/src/` where SQL/DataFusion planning already occurs.
- Emit pushdown payload into scan orchestration (`crates/logfwd-arrow/src/scanner.rs`) and then `ScanConfig`.
- Preserve original filter in DataFusion until conformance tests prove exact semantics.

---

## 6) File-level insertion plan (mapped to requested files)

> Note: line numbers cannot be provided from this checkout because these files are not present here. Paths below are the intended insertion points from the issue specification.

1. **`crates/logfwd-core/src/scan_config.rs`**
   - Add `row_predicate: Option<ScanPredicateIR>`.
   - Add `predicate_fields: SmallVec<FieldId>` (or equivalent) to drive minimal extraction.

2. **`crates/logfwd-core/src/json_scanner.rs`** (`scan_streaming()`)
   - Before full extraction: parse only fields needed for predicate.
   - Evaluate IR with tri-state semantics.
   - Skip non-matching rows before `streaming_builder.begin_row()`.

3. **`crates/logfwd-arrow/src/scanner.rs`** (`scan()` / `scan_detached()`)
   - Thread predicate config from SQL plan into core `ScanConfig`.
   - Keep residual filter metadata for downstream DataFusion safety.

4. **`crates/logfwd-arrow/src/streaming_builder/mod.rs`**
   - No primary logic change required if scanner short-circuits rows.
   - Optional: add counters (`rows_skipped_predicate`) for observability.

5. **`crates/logfwd-transform/src/**`**
   - Add DataFusion `Expr` -> `ScanPredicateIR` lowering.
   - Add filter pushdown capability reporting (exact vs unsupported).

---

## 7) Predicate semantics details

## 7.1 SQL three-valued logic compatibility

For pushdown correctness:

- Missing field acts as `NULL`.
- `NULL = 'x'` => `Unknown` (treated as row filtered out in `WHERE`).
- `IS NULL` / `IS NOT NULL` must be explicit and exact.
- `OR`/`AND` truth tables must preserve unknown propagation.

## 7.2 Type behavior

- If field type is known from schema, compare in native type path.
- If unknown/dynamic JSON number parsing needed, define deterministic coercion rules.
- On non-coercible type mismatch, yield `Unknown` (not `False`) to align with SQL behavior.

---

## 8) Performance gain estimate

Let:

- `p = selectivity` (fraction of rows that pass)
- `Cparse = cost to decode line + predicate fields`
- `Cextract = cost to decode non-predicate projected fields`
- `Cbuild = Arrow append/allocation per row`

### Baseline (no row pushdown)

`Cost_base ~= N * (Cparse + Cextract + Cbuild)`

### With Option A pushdown

`Cost_push ~= N * (Cparse + p * (Cextract + Cbuild))`

### Fraction of removable work

`Saved ~= (1 - p) * (Cextract + Cbuild) / (Cparse + Cextract + Cbuild)`

#### Example bands

- If `p=0.10` and `Cextract+Cbuild` is ~70% of row cost, savings ≈ **63%** total scan CPU.
- If `p=0.01` and same ratio, savings ≈ **69%**.
- If `p=0.50`, savings ≈ **35%**.

So expected gains are large whenever filters are selective and projected rows are wide.

---

## 9) Ordered implementation plan

1. **Add IR + config plumbing**
   - Define `ScanPredicateIR` and add to `ScanConfig`.
2. **Implement DataFusion lowering**
   - Translate supported expressions to IR; classify unsupported nodes.
3. **Scanner pre-check path**
   - Minimal field extraction for predicate evaluation.
   - Short-circuit row drop before builder row lifecycle.
4. **Residual filter correctness path**
   - Keep DataFusion residual filter active for unsupported/uncertain predicates.
5. **Metrics and visibility**
   - Add counters: scanned rows, predicate-evaluated rows, rows dropped by pushdown.
6. **Conformance tests**
   - SQL parity tests for null semantics, numeric coercion, AND/OR behavior.
7. **Benchmark pass**
   - Synthetic and real workloads with pass-rate sweeps (1%, 10%, 50%, 90%).

---

## 10) Risks and mitigations

1. **Semantic drift vs SQL/DataFusion**
   - *Risk:* pushdown evaluator disagrees on null/type corner cases.
   - *Mitigation:* keep residual filters initially; ship only exact-safe predicates first.

2. **JSON dynamic typing complexity**
   - *Risk:* numeric/string coercions can become expensive or inconsistent.
   - *Mitigation:* strict, documented coercion matrix; fall back to non-pushdown on ambiguity.

3. **Planner/evaluator complexity creep**
   - *Risk:* supporting many expression types too early.
   - *Mitigation:* staged rollout with explicit non-goals.

4. **Observability blind spots**
   - *Risk:* difficult to verify pushdown is active/effective.
   - *Mitigation:* expose scan metrics and debug logs for pushdown decisions.

---

## 11) Non-goals (v1)

- Full SQL expression pushdown parity.
- Regex/LIKE/function pushdown.
- Nested JSON path optimizer beyond top-level fields.
- Rewriting DataFusion execution model; objective is source-side row skipping only.

---

## 12) Recommended rollout

1. **Phase 1 (safe subset):** `=`, `!=`, numeric comparisons, `IS NULL`, `IS NOT NULL`, `AND`.
2. **Phase 2:** `OR`, `NOT`, literal `IN` lowering.
3. **Phase 3:** tune parser/extraction micro-optimizations and add predicate cost heuristics.

Success criteria:

- No query result diffs on conformance corpus.
- Meaningful scan CPU reductions on selective filters.
- Stable memory reduction from fewer builder appends.

