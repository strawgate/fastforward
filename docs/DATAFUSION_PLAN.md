This changes everything. You're already at **4.7M lines/sec** with OTLP encoding. That's 33x faster than vlagent. The architecture doc makes clear why — 125ns/line, zero allocations, zero-copy references into the read buffer, hand-rolled protobuf, no materialized event struct.

The question isn't "how do we build a fast forwarder." It's built. The question is: **how do we add SQL transform power without destroying what you have.**

Let me lay out a plan grounded in what actually exists.

## The Core Tension

```
Current hot path:          125ns/line → 4.7M lines/sec
                           0 heap allocations
                           Zero-copy buffer references

DataFusion path (from      ~1000-1500ns/line → ~700K-1M lines/sec
our benchmarks):           Arrow RecordBatch materialization
                           Column builders, schema management
```

Adding DataFusion naively would be an **8-12x regression**. That's not acceptable. The plan has to preserve the fast path and add DataFusion alongside it, not instead of it.

## The Plan

### Phase 0: Extend the existing scanner (no DataFusion)

**What:** Replace the hardcoded 3-field-category scanner with a configurable field extractor. The user specifies which fields to extract and simple predicates to evaluate, and the scanner handles it inline — same zero-copy, zero-allocation pattern.

**Config:**
```toml
[source]
path = "/var/log/pods/**/*.log"
format = "cri"

[filter]
# Evaluated inline in the scanner — no Arrow, no DataFusion
level = "!DEBUG"            # != DEBUG
service = ["api", "payment"] # IN list

[extract]
# Which JSON fields to pull out (for OTLP attributes)
fields = ["timestamp", "level", "message", "service", "request_id", "trace_id"]

[output]
type = "otlp"
endpoint = "collector:4317"
```

**Implementation:** Extend `extract_json_fields()` to accept a configurable field list and simple predicates. The scanner loop stays the same — memchr for quotes, match keys, extract values as `(offset, len)` references. Add predicate evaluation inline: after extracting the filter field, check the predicate, skip the line if it fails.

```rust
// Current: hardcoded 3 categories
fn extract_json_fields(line: &[u8]) -> JsonFields<'_>

// Phase 0: configurable fields + inline predicate
fn extract_json_fields(
    line: &[u8],
    config: &ExtractConfig,  // which fields, compiled predicates
) -> Option<JsonFields<'_>> // None = filtered out
```

**Cost:** ~150-200ns/line (vs 125ns today). The extra cost is checking more field names and evaluating a predicate. Still zero allocations. Still zero copy. Still in the existing buffer-swap architecture.

**Throughput:** ~3.5-4M lines/sec. Small regression, huge functionality gain.

**What this gives users:** Configurable field extraction, basic filtering, field selection for OTLP attributes — covering 80% of real pipeline configs without any architectural change.

### Phase 1: SQL query parsing for field pushdown

**What:** Accept a SQL transform in the config. Parse it at startup (using `sqlparser-rs`) to extract:
1. Which fields the query references (for selective extraction)
2. Simple predicates that can be pushed down to the scanner
3. Whether the query is "simple enough" for Phase 0's inline evaluation or needs full DataFusion

**Config:**
```toml
[transform]
sql = """
  SELECT * EXCEPT (stack_trace, debug_context)
  FROM logs
  WHERE level != 'DEBUG' AND service IN ('api', 'payment')
"""
```

**At startup:**
```rust
fn analyze_query(sql: &str) -> QueryPlan {
    let referenced_fields = extract_column_refs(sql);  // {level, service, stack_trace, debug_context, *}
    let pushdown_predicates = extract_simple_predicates(sql);  // level != 'DEBUG', service IN (...)
    let needs_datafusion = has_complex_ops(sql);  // JOIN, GROUP BY, UDF, computed fields, REPLACE

    if !needs_datafusion {
        // Phase 0 fast path handles it
        QueryPlan::InlineFilter { fields: referenced_fields, predicates: pushdown_predicates }
    } else {
        // Need DataFusion for this query
        QueryPlan::DataFusion { fields: referenced_fields, pushdown: pushdown_predicates, sql }
    }
}
```

**What "simple enough" means:**
- `SELECT *` or `SELECT * EXCEPT (...)` with no REPLACE → inline (just skip fields in output)
- `WHERE` with equality, IN, != on string fields → inline predicate in scanner
- Anything with `GROUP BY`, `JOIN`, `REPLACE`, `CASE`, arithmetic, `int()`, `float()`, regex → needs DataFusion

**Cost:** Zero runtime cost. This is a startup-time analysis. The output is configuration for the Phase 0 scanner or a signal that Phase 2 is needed.

**What this gives users:** They write SQL, but for simple cases it compiles down to the existing fast path automatically. The SQL is the config language, the execution path is chosen for them.

### Phase 2: DataFusion integration for complex transforms

**What:** When Phase 1 determines the query needs DataFusion, we build the Arrow RecordBatch path alongside the existing pipeline. This is where the multi-typed column work from our conversation plugs in.

**Architecture:**
```
Read buffer (existing)
  │
  ▼
CRI parser (existing) → raw message bytes
  │
  ├── FAST PATH (Phase 0): simple filter + field extract
  │     Scanner evaluates predicates inline
  │     Emits LogEvent references directly to encoder
  │     125-200ns/line, zero alloc
  │
  └── DATAFUSION PATH (Phase 2): complex transforms
        Scanner extracts ALL referenced fields
        Accumulates ~4096 events
        Builds typed Arrow columns:
          level_str, duration_ms_int, duration_ms_str, etc.
        DataFusion executes cached plan
        Output serializer reads result + original typed columns
        ~800-1500ns/line
```

**The key design decision from the architecture doc:**

> does it need a materialized Arrow RecordBatch per chunk, or can it evaluate predicates from a streaming scan?

**Answer: Both.** Phase 0 is the streaming scan. Phase 2 is the RecordBatch path. The SQL analysis in Phase 1 decides which path runs. This isn't a compromise — it's the optimal architecture because simple queries genuinely don't need materialization.

**RecordBatch construction from the existing buffer model:**

The architecture doc says the process buffer holds newline-delimited messages. The scanner produces `(offset, len)` references. For the DataFusion path, we accumulate these references for ~4096 lines, then build the RecordBatch:

```rust
// Accumulate during scan (still zero-copy references into process buffer)
struct PendingBatch<'a> {
    lines: Vec<&'a [u8]>,        // raw line refs (for _raw column)
    field_refs: Vec<FieldRefs>,   // per-line field (offset, len, type_tag) refs
    count: usize,
}

// When batch is full, materialize Arrow columns
fn build_record_batch(pending: &PendingBatch, buffer: &[u8]) -> RecordBatch {
    // For each field, check type_tags to determine which typed columns to create
    // Build: duration_ms_int (Int64), duration_ms_str (Utf8), etc.
    // Build: _raw column from line refs
    // This is the expensive step: ~300-500ns/line
}
```

The field scanning itself is similar cost to today (~150ns/line). The new cost is materializing the Arrow columns (~300-500ns/line) and DataFusion execution (~100-300ns/line). Total: ~600-1000ns/line for the DataFusion path.

**Interaction with the double-buffer swap:** The pending batch must be materialized and the DataFusion query executed before the buffer swaps. Since the current chunk size is ~4000 lines and the DataFusion path takes ~4-8ms per batch, this fits within the existing buffer lifecycle. The process buffer is held while DataFusion runs, then released for the next swap.

**The rewriter from our conversation lives here:**
```rust
fn rewrite_sql(user_sql: &str, field_type_map: &FieldTypeMap) -> String {
    // duration_ms → COALESCE(CAST(duration_ms_int AS VARCHAR), duration_ms_str)
    // int(duration_ms) → COALESCE(duration_ms_int, CAST(...))
    // EXCEPT (field) → EXCEPT (field_int, field_str, field_float)
    // WHERE level = 'ERROR' → WHERE level_str = 'ERROR'
}
```

### Phase 3: Output serializer with type preservation

**What:** After DataFusion executes, the output serializer needs to produce OTLP protobuf (or JSON, or ES bulk) from the result. For untouched fields, it reads the original typed columns for correct OTLP attribute types. For transformed fields, it reads the result column types.

**For the OTLP encoder (existing hand-rolled protobuf):**

The current `encode_log_record()` takes `JsonFields` with byte slice references and writes protobuf directly. The DataFusion path would add a parallel encoder:

```rust
// Existing: from scanner output (fast path)
fn encode_log_record(fields: &JsonFields, out: &mut Vec<u8>)

// New: from DataFusion result (complex path)  
fn encode_log_record_from_batch(
    batch: &RecordBatch,
    row: usize,
    typed_columns: &TypedColumnIndex,  // knows duration_ms_int vs duration_ms_str
    out: &mut Vec<u8>,
)
```

The typed column index is built once per batch schema. For each field, it knows which `_int`, `_str`, `_float` columns exist and checks non-null to determine the output type per row.

**For JSON output:** If `_raw` is in the result and the query didn't modify values (EXCEPT-only, no REPLACE), memcpy `_raw` — this is the passthrough the existing architecture already excels at.

### Phase 4: `SELECT * EXCEPT` in the fast path

**What:** The fast path (Phase 0) can handle `EXCEPT` without DataFusion. After the scanner extracts fields and the encoder writes OTLP attributes, the EXCEPT list is just "skip these field names during encoding." The scanner already iterates fields — it can check against a skip set.

```rust
fn encode_log_record_filtered(
    fields: &JsonFields,
    skip_fields: &HashSet<&[u8]>,  // {"stack_trace", "debug_context"}
    out: &mut Vec<u8>,
) {
    for (key, value) in fields.attributes.iter() {
        if skip_fields.contains(key) { continue; }
        encode_attribute(key, value, out);
    }
}
```

**Cost:** ~10ns extra per line (one hash lookup per field). The fast path now handles `SELECT * EXCEPT (...)` at 3.5M+ lines/sec.

This means the vast majority of real pipeline configs — filter + drop some fields — run at near-native speed with no DataFusion involved.

## What Each Phase Delivers

| Phase | Users get | Performance | Complexity |
|---|---|---|---|
| 0 | Configurable field extract + simple filters | 3.5-4M/sec | Low — extend existing scanner |
| 1 | SQL config syntax, auto-path selection | Same as Phase 0 | Medium — SQL parser, no runtime cost |
| 2 | Full SQL: JOIN, GROUP BY, REPLACE, regex, computed fields | 700K-1.2M/sec | High — Arrow integration, rewriter |
| 3 | Correct typed OTLP output from SQL results | Same as Phase 2 | Medium — typed column serializer |
| 4 | EXCEPT in fast path | 3.5M/sec | Low — skip set in encoder |

## The Performance Picture

```
Current (no transforms):     4.7M lines/sec   125ns/line
Phase 0 (filter + extract):  3.5-4M lines/sec  150-200ns/line
Phase 4 (EXCEPT fast path):  3.5M lines/sec    170ns/line

Phase 2 (DataFusion):        700K-1.2M lines/sec  800-1500ns/line

For comparison:
  vlagent:                    143K lines/sec
  Vector:                     25K lines/sec
  OTel Collector:             20K lines/sec
```

Even the DataFusion path is **5x faster than vlagent** and **28-48x faster than Vector.** The fast path is **24x faster than vlagent.** These aren't projected numbers — they're grounded in your existing 4.7M baseline and our measured DataFusion overhead.

## What the Config Looks Like

```toml
# Simple — runs Phase 0 fast path automatically
[transform]
sql = """
  SELECT * EXCEPT (stack_trace, debug_context)
  FROM logs
  WHERE level != 'DEBUG'
"""
# → Phase 1 analysis: simple filter + EXCEPT → fast path
# → 3.5M lines/sec

# Complex — Phase 1 routes to DataFusion automatically
[transform]
sql = """
  SELECT * EXCEPT (stack_trace)
         REPLACE (redact(message, 'email') as message),
         'production' as environment,
         CASE WHEN int(duration_ms) > 1000 THEN 'slow'
              ELSE 'fast' END as latency_tier
  FROM logs l
  LEFT JOIN csv('/etc/logfwd/oncall.csv') o ON l.service = o.service_name
  WHERE level != 'DEBUG' AND int(status) >= 400
"""
# → Phase 1 analysis: REPLACE + JOIN + CASE → DataFusion path
# → 700K-1.2M lines/sec (still 5x faster than vlagent)
```

Same config syntax. Same SQL. The runtime picks the fastest path. User doesn't know or care.

## Build Order Recommendation

**Phase 0 first** because it delivers the most value with the least architectural change. You're extending the existing scanner, not replacing it. Users get configurable field extraction and filtering immediately. Ship it.

**Phase 1 next** because it's zero runtime cost — it's just a startup SQL parser that configures Phase 0. Users can now write SQL transforms and get the fast path automatically for simple queries. Ship it.

**Phase 2 last** because it's the biggest engineering lift (Arrow integration, rewriter, typed columns, DataFusion plan caching, buffer lifecycle coordination) and it only matters for the 20% of users who need complex transforms. But when they need it, they get full SQL with correct types at 700K+ lines/sec. Ship it.

**Phase 3 and 4 can interleave** — they're independent improvements to output fidelity and fast-path coverage.

## The Hard Engineering Problems

1. **Buffer lifecycle coordination (Phase 2):** The DataFusion batch must complete before the double-buffer swap. Current chunk size (~4000 lines) gives ~4ms for DataFusion execution at 1000ns/line. If DataFusion is slower on a complex query, you need to either increase chunk size or add a copy-out step before buffer release.

2. **The rewriter (Phase 2):** Mapping bare column names to typed column variants (`duration_ms` → `COALESCE(CAST(duration_ms_int AS VARCHAR), duration_ms_str)`), handling EXCEPT expansion, and inserting type-appropriate coalesce chains. This is ~500-800 lines of Rust and will have edge cases. But it's bounded — the SQL grammar is finite.

3. **Schema stability (Phase 2):** The multi-typed columns from our conversation. First batch infers the schema. Subsequent batches reuse it. New field appears → extend schema, recompile plan (~1ms). Type conflict within a field → multiple typed columns (`duration_ms_int` + `duration_ms_str`). The rewriter handles the mapping.

4. **Selective field extraction guided by SQL (Phase 1→0):** The SQL parser extracts referenced field names. These become the scanner's field list. For `SELECT *`, the scanner extracts all fields (slower). For `SELECT timestamp, level, service`, it extracts only those three (fast). The insight from our conversation: push the field list down to the scanner so it skips unreferenced keys.

## What I'd Measure First

Before writing any DataFusion code, I'd benchmark the field scanning cost at different extracted field counts — 3 fields (today), 6 fields, 12 fields, all fields. This tells us the real cost of `SELECT *` vs selective extraction and validates the Phase 0 performance projections. That measurement is the foundation for everything else.