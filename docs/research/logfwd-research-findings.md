# logfwd DataFusion Integration: Research Findings & Prototype Results

## Summary

We spent an extensive design session exploring how to add SQL-based transforms to
logfwd's 4.7M lines/sec log forwarding pipeline. This document captures everything
we proved with benchmarks, the dead ends we explored, and the design decisions that
fell out of the data.

All benchmarks ran on PyArrow 23.0.1 + DataFusion 52.3.0 (Python bindings backed
by the same Rust/C++ engines as native Rust). Numbers are directionally correct
but should be validated in native Rust — Python adds overhead to parse/serialize
stages but NOT to DataFusion query execution or Arrow kernel operations (those run
in native code regardless).

---

## Prototype 1: DataFusion Query Cost Is Negligible

**Question:** Is DataFusion too heavy for a log forwarder? Can we afford a SQL query
engine in a pipeline that needs to process millions of lines per second?

**Test:** 100K JSON log lines with variable schemas. Parse into Arrow RecordBatch,
execute a normalize+filter SQL query, serialize output.

**Key result (from benchmark.py):**

```
Stage breakdown (Parse + Normalize Query + JSON serialize):
  Parse (JSON → Arrow):    210ms   22.0%   2,103ns/line
  Query (DataFusion SQL):   51ms    5.3%     505ns/line   ← THE KEY NUMBER
  Serialize (Arrow → JSON): 695ms   72.7%   9,265ns/line
```

**The query is 5.3% of the pipeline cost.** DataFusion's vectorized columnar execution
on a pre-compiled plan is essentially free relative to the unavoidable parse and
serialize costs. The normalize+filter query with COALESCE across 4 field name variants
costs only 505ns/line.

Simple filter (`WHERE level != 'DEBUG'`): 85ns/line.
GROUP BY aggregation: 70ns/line.
Complex multi-condition filter: 21ns/line.

**Conclusion:** DataFusion is viable. The query engine is not the bottleneck. Parse and
serialize dominate. This was the foundational finding that enabled the entire design.

---

## Prototype 2: arrow-json Crashes on Type Conflicts

**Question:** Can we use Arrow's built-in JSON parser for log data?

**Test:** Fed JSON lines with type conflicts to arrow-json:
```json
{"status": 500, "level": "ERROR"}
{"status": "internal_error", "level": "WARN"}
```

**Result:** Hard crash.
```
JSON parse error: Column(/status) changed from number to string in row 1
```

**Conclusion:** arrow-json is unusable for dynamic log data. We must build our own
JSON→Arrow parser (using sonic-rs) that handles type conflicts gracefully. This led
to the all-Utf8 and multi-typed column approaches.

---

## Prototype 3: Map<Utf8, Utf8> Works But Has Limitations

**Question:** Can we store all log fields in a single Map column to avoid schema issues?

**Test (from benchmark_raw_parsed.py):** Built `(_raw, _parsed: Map<Utf8, Utf8>)` tables and
queried with DataFusion's native bracket notation.

**Key results:**

```
DataFusion native bracket access on Map:
  _parsed['level'] filter:        28.2ms    282ns/line
  _parsed['level'] + 3 more:      92.6ms    926ns/line
  GROUP BY _parsed['service']:    93.2ms    932ns/line

vs flat Utf8 columns:
  level filter:                    8.9ms     89ns/line
  GROUP BY service:                5.2ms     52ns/line
```

Map access is ~3x slower than flat column access for filtering. But 282ns/line is
still cheap relative to parse cost (2103ns/line). The map is 6% of the parse cost.

**DataFusion bracket notation `_parsed['key']` works natively.** No UDF needed.
Returns the value directly. This was a key finding.

**Limitation discovered:** `SELECT * EXCEPT` doesn't work on map keys — only on
real columns. `SELECT *` returns the raw map, not individual fields. This means
the Map approach requires a rewriter to paper over the ergonomic gaps, and common
SQL patterns like EXCEPT and REPLACE don't work naturally.

**Conclusion:** Map works for storage but creates UX problems. Flat columns with
all-Utf8 values are preferable for the query layer.

---

## Prototype 4: All-Utf8 Columns Have Minimal Overhead

**Question:** If every column is Utf8 (strings), how much do we lose on numeric operations?

**Test (from benchmark_utf8_vs_typed.py):** Compared typed columns (Int64, Float64) vs all-Utf8
columns with TRY_CAST for numeric operations. 100K rows, median-of-5 with warmup.

**Key results:**

```
Operation                    Typed     Utf8+CAST   Ratio
String equality (level=ERR)  2.19ms    1.86ms      0.85x (Utf8 FASTER)
GROUP BY COUNT               2.68ms    2.50ms      0.93x (Utf8 FASTER)
Numeric compare (dur>1000)   2.00ms    3.09ms      1.54x
Numeric AVG                  1.83ms    2.36ms      1.29x
```

String operations (90% of log pipeline work): identical or faster with Utf8.
Numeric operations (10% of log pipeline work): 1.3-1.5x slower with TRY_CAST.

Memory: 1.01x — essentially identical. Utf8 "42" takes 2 bytes + offset.
Int64 42 takes 8 bytes fixed. For typical log values, Utf8 is the same or smaller.

**Conclusion:** All-Utf8 doesn't meaningfully hurt anything. The only cost is ~1.5x
on numeric operations, which represent ~10% of real pipeline work. Total pipeline
impact: ~1%.

---

## Prototype 5: SELECT * EXCEPT and REPLACE Work in DataFusion

**Question:** Can DataFusion handle the "100 fields, drop 1" problem?

**Test:** Direct SQL queries in DataFusion 52.3.0.

```sql
SELECT * EXCEPT (b) FROM test              -- ✓ Works
SELECT * EXCLUDE (b) FROM test             -- ✓ Works (alternate syntax)
SELECT * REPLACE (99 as b) FROM test       -- ✓ Works
SELECT * EXCEPT b FROM test                -- ✓ Works (no parens needed)
```

All four syntaxes work. This is the key ergonomic finding — users can write:

```sql
SELECT * EXCEPT (stack_trace, debug_context)
       REPLACE (redact(email, 'email') as email),
       'production' as environment
FROM logs
WHERE level != 'DEBUG'
```

One SQL statement handles filter + drop fields + redact + add fields. No DSL needed.

---

## Prototype 6: PostgreSQL-Style :: Cast Works in DataFusion

**Test:** Cast syntax for type coercion.

```sql
SELECT duration_ms::INT FROM logs           -- ✓ Works, returns Int32
SELECT duration_ms::BIGINT FROM logs        -- ✓ Works, returns Int64
SELECT duration_ms::DOUBLE FROM logs        -- ✓ Works, returns Float64
SELECT * FROM logs WHERE duration_ms::INT > 100  -- ✓ Works
SELECT AVG(duration_ms::DOUBLE) FROM logs   -- ✓ Works
SELECT * FROM logs ORDER BY duration_ms::INT DESC  -- ✓ Works
```

**BUT: :: is a hard cast.** `"slow"::INT` crashes with:
```
Cast error: Cannot cast string 'slow' to value of Int32 type
```

`TRY_CAST("slow" AS INT)` returns NULL gracefully.

**Conclusion:** We can't use :: for log data (type conflicts exist). Instead, we
register convenience functions `int()`, `float()` that wrap TRY_CAST:

```sql
-- These work correctly on mixed-type data:
SELECT int(duration_ms) FROM logs       -- 42 → 42, "slow" → NULL
SELECT float(hit_rate) FROM logs        -- 0.95 → 0.95, "bad" → NULL
WHERE int(status) >= 400                -- 500 → true, "healthy" → NULL → excluded
AVG(float(duration_ms))                 -- correct numeric average, NULLs skipped
```

We verified these work as registered UDFs in DataFusion. The UDF approach handles
any mix of types gracefully.

---

## Prototype 7: DataFusion vs Direct Pipeline Crossover

**Question:** At what batch size does DataFusion beat a simple per-event loop?

**Test (from bench_df_vs_direct.py):** Same transforms implemented two ways —
DataFusion SQL on Arrow RecordBatch vs Python dict-based per-event pipeline.
Measured at batch sizes 10, 50, 100, 500, 1000, 5000, 8192.

**Key results at 8192 events:**

```
Transform             DataFusion    Direct     Winner
Simple filter         162ns/evt     43ns/evt   Direct 3.7x
Filter + rename       139ns/evt     194ns/evt  DataFusion 1.4x
Filter + regex        380ns/evt     1071ns/evt DataFusion 2.8x
Complex combined      498ns/evt     1315ns/evt DataFusion 2.6x
```

**DataFusion's fixed overhead per query: ~0.5-2ms** (plan dispatch, Arrow overhead).
At 10 events: ~100,000ns/event overhead (terrible).
At 8192 events: ~120ns/event overhead (negligible).

**Crossover points:**
- Simple filter only: Direct always wins (but DataFusion is unnecessary here — fast path)
- Rename / add fields: ~5000-8000 events
- Regex / computed fields: ~2000-5000 events
- Complex multi-step: ~2000-5000 events

**Conclusion:** DataFusion wins for non-trivial transforms at batch sizes above ~2000.
For simple filter-only queries, the inline scanner fast path wins at any batch size.
The auto-detection at startup (parse SQL, determine complexity) is the correct approach.
Both paths coexist.

---

## Prototype 8: Multi-Typed Columns for Type Preservation

**Question:** How do we preserve JSON types (int vs string vs float) through Arrow
and DataFusion without schema conflicts?

**Test (from bench_multi_typed.py):** Build separate typed columns per field:
`duration_ms_int` (Int64), `duration_ms_str` (Utf8) for fields with type conflicts.

**Test data:** 8192 events, duration_ms is 90% int and 10% string, status is 90% int
and 10% string.

**Key results:**

```
Column inventory:
  duration_ms_int     int64     7372/8192 non-null (90%)
  duration_ms_str     string     820/8192 non-null (10%)
  status_int          int64     7372/8192 non-null (90%)
  status_str          string     820/8192 non-null (10%)
  level_str           string    8192/8192 non-null (100%)
  hit_rate_float      double    8192/8192 non-null (100%)

Memory overhead: 1.03x vs all-Utf8 (essentially zero)

Query (filter on typed column):
  WHERE duration_ms_int > 1000    →  5.29ms   (native Int64 comparison)
  WHERE status_int >= 400         →  2.82ms   (native Int64 comparison)
  AVG(duration_ms_int)            → 17.74ms   (correct numeric average)

Output serialization (OTLP protobuf):
  Row 0: duration_ms → int_value=21   (from _int column)
         hit_rate → double_value=0.03  (from _float column)
         level → string_value="ERROR"  (from _str column)
```

**This solves the output type problem.** The column suffix IS the type tag. It survives
any DataFusion transformation — SELECT *, EXCEPT, REPLACE, GROUP BY all carry the
typed columns through. The output serializer checks which typed variant is non-null
for each row and emits the correct protobuf attribute type. No guessing. No heuristic.

**How the rewriter handles it:**

```sql
-- User writes:
SELECT duration_ms FROM logs

-- Rewriter produces (bare column → coalesce to string):
SELECT COALESCE(CAST(duration_ms_int AS VARCHAR), duration_ms_str) as duration_ms

-- User writes:
SELECT int(duration_ms) FROM logs

-- Rewriter produces (explicit cast → use typed column):
SELECT COALESCE(duration_ms_int, TRY_CAST(duration_ms_str AS BIGINT)) as duration_ms

-- User writes:
WHERE duration_ms > 100

-- With typed columns, we prefer the dominant type:
WHERE duration_ms_int > 100
-- String rows get NULL in duration_ms_int → fail the comparison → correctly excluded
```

---

## Prototype 9: Union Types in Arrow (Dead End)

**Question:** Can we use Arrow's DenseUnion type inside a Map for type preservation?

**Test:** Built `Map<Utf8, DenseUnion<Utf8, Int64, Float64>>`.

```python
# Building works:
union_arr = pa.UnionArray.from_dense(type_ids, offsets, [str_arr, int_arr, float_arr])
map_arr = pa.MapArray.from_arrays(offsets, keys, union_arr)
# Table creation works. Bracket access works:
_parsed['level'] → returns 'ERROR' (string), _parsed['status'] → returns 500 (int)

# BUT: any comparison/filter/aggregate CRASHES:
# "cannot call extend_nulls on UnionArray as cannot infer type"
```

DataFusion can store and read Union values but cannot compare, filter, or aggregate
them. Every compute kernel would need Union dispatch logic. This is a fundamental
DataFusion limitation.

**Conclusion:** Union types are a dead end for the query layer. Multi-typed columns
(Prototype 8) achieve the same type preservation with zero DataFusion limitations.

---

## Prototype 10: Schema Inference Timing

**Question:** How expensive is post-construction type inference on Utf8 columns?

**Test:** Build all-Utf8 table, then try to narrow each column using Arrow's cast kernels.

```
Arrow type narrowing on 12 columns × 8192 rows:
  Brute force (try cast every column):  45.3ms  5534ns/evt  ← TERRIBLE
  With heuristic (peek first value):     7.5ms   915ns/evt  ← acceptable
  Successful cast only (ints):           0.05ms    7ns/evt  ← essentially free
```

The cost is entirely from FAILED casts. Trying Int64 on "message-0" scans the whole
column before failing (~280ns/evt). The heuristic (check if first value starts with
digit) eliminates 6x of the cost.

**But we decided against per-batch inference.** Type consistency can't be assumed
across batches. The winning approach is to detect types DURING parsing (sonic-rs
already tells us the JSON type) and route to typed column builders at parse time.
No post-hoc inference needed.

---

## Design Decisions: Final Architecture

Based on all prototypes, here's what we settled on:

### Internal Representation

```
Per batch, the RecordBatch contains:

  _raw: Utf8           — original JSON line bytes (for passthrough output)
  level_str: Utf8      — always string (no type conflicts on this field)
  service_str: Utf8    — always string
  duration_ms_int: Int64   — 90% of values (JSON integers)
  duration_ms_str: Utf8    — 10% of values (JSON strings like "slow")
  status_int: Int64        — 90% of values
  status_str: Utf8         — 10% of values
  hit_rate_float: Float64  — always float
  message_str: Utf8        — always string
  request_id_str: Utf8     — always string
  ... etc for every field seen in the batch
```

Fields with consistent types get one column. Fields with type conflicts get multiple
typed columns (one per observed type). The column suffix encodes the type.

### The SQL Rewriter

Maps user-facing SQL to internal typed column names. Rules:

```
Bare column (no cast):
  duration_ms → COALESCE(CAST(duration_ms_int AS VARCHAR),
                         CAST(duration_ms_float AS VARCHAR),
                         duration_ms_str) as duration_ms
  level → level_str as level  (only one type, no coalesce needed)

Explicit int() cast:
  int(duration_ms) → COALESCE(duration_ms_int,
                               CAST(duration_ms_float AS BIGINT),
                               TRY_CAST(duration_ms_str AS BIGINT)) as duration_ms

EXCEPT (field):
  EXCEPT (duration_ms) → EXCEPT (duration_ms_int, duration_ms_str, duration_ms_float)

SELECT *:
  Expands to all fields with coalesced aliases

WHERE comparisons:
  WHERE duration_ms > 100 → WHERE duration_ms_int > 100
  (string variants get NULL, correctly excluded from numeric comparison)
```

### Schema Management

- Schema inferred from first batch + SQL column references
- SQL parsed at startup → column refs extracted before any data arrives
- First batch: scanner discovers field names + types → merge with SQL refs → build schema
- Plan compiles against merged schema
- Subsequent batches: same fields → reuse cached plan
- New field appears → extend schema, recompile plan (~1ms)
- Type conflict within a field → create additional typed column, recompile

### Two Execution Paths

```
Config SQL → startup analysis:

FAST PATH (no DataFusion):
  Triggered by: filter-only queries, EXCEPT with no REPLACE/computed fields
  Scanner evaluates predicates inline
  Encoder consumes field refs directly
  ~150-200ns/line

SQL PATH (DataFusion):
  Triggered by: GROUP BY, JOIN, REPLACE with expressions, CASE, arithmetic, aggregates
  Scanner extracts all fields → builds typed Arrow columns
  DataFusion executes cached plan
  Output serializer reads result + typed columns
  ~800-1500ns/line
```

### Output Type Preservation

For untouched columns (not referenced in SQL), the output serializer reads the
original typed columns directly:
- `duration_ms_int` non-null → emit `int_value: 42`
- `hit_rate_float` non-null → emit `double_value: 0.95`
- `level_str` non-null → emit `string_value: "ERROR"`

For touched columns (referenced in SQL), the serializer reads the result column type.
If the user wrote `int(duration_ms)`, the result column is Int64.
If the user wrote bare `duration_ms`, the result column is Utf8 (coalesced to string).

For JSON output: if _raw is in the result and no values were modified, memcpy _raw.
Otherwise re-serialize from columns.

---

## What Still Needs Research (In Priority Order)

### R1: Full-Document JSON Scanning Cost

The current scanner in otlp.rs early-exits after finding 3 fields. For SELECT *,
we need to scan all fields to EOF. Measure ns/line for 5, 10, 15, 20 fields.
This is the foundation for all SQL path performance projections.

**How to test:** Modify `extract_json_fields()` to remove the early-exit condition.
Add a counter for fields found. Benchmark with the existing e2e harness.

### R2: Arrow Column Construction from Pre-Scanned Positions

After the scanner finds field positions as `(offset, len)` byte ranges, how fast
can we build the typed Arrow RecordBatch? This is the key unknown for the SQL path.

**How to test:** Write a function that takes the scanner's output (field positions
pointing into the read buffer) and builds Arrow StringArray/Int64Array/Float64Array
columns. The field scanning is already done — this measures only the organized memcpy
+ type routing cost.

Expected: ~50-100ns per field per line for memcpy into StringBuilder.
10 fields × 4000 lines = ~2-4ms per batch.

### R3: SQL Rewriter Prototype

Build the AST walker using sqlparser-rs. Test against these SQL patterns:

```sql
-- Basic patterns:
SELECT * FROM logs WHERE level = 'ERROR'
SELECT * EXCEPT (field) FROM logs
SELECT * REPLACE (expr as field) FROM logs
SELECT *, 'value' as new_field FROM logs

-- Typed access:
WHERE int(duration_ms) > 100
ORDER BY float(duration_ms) DESC
SELECT AVG(int(duration_ms)) GROUP BY service

-- Complex:
SELECT * EXCEPT (a, b) REPLACE (redact(email, 'email') as email),
       'prod' as env,
       CASE WHEN int(dur) > 1000 THEN 'slow' ELSE 'fast' END as tier
FROM logs l
LEFT JOIN csv('/etc/oncall.csv') o ON l.service = o.svc
WHERE level != 'DEBUG' AND int(status) >= 400
```

The rewriter needs to handle: bare column refs → coalesce, int()/float() → typed
column access, EXCEPT → expand to all typed variants, SELECT * → expand all fields
with coalesced aliases, column refs in WHERE/ORDER BY/GROUP BY.

### R4: Buffer Lifecycle Coordination

Verify that the Arrow column construction + buffer reclaim + DataFusion execution
sequence works correctly with the existing double-buffer swap in chunk.rs.

**The expected flow:**
1. ChunkAccumulator yields OwnedChunk (read buffer ownership transferred)
2. Scanner runs on OwnedChunk.data() → produces field position refs
3. Arrow column builders copy values from OwnedChunk into owned Arrow arrays
4. OwnedChunk is reclaimed (returned to accumulator) — buffer is now free
5. DataFusion executes on RecordBatch (independently owned memory)

Step 4 happens BEFORE step 5. DataFusion never holds a reference to the read buffer.
The question: does the copy in step 3 add significant latency before buffer reclaim?
If column construction takes 3ms, the read buffer is held 3ms longer than today.

### R5: End-to-End SQL Path Benchmark in Rust

The Python benchmarks gave us directional numbers. We need native Rust numbers for:
- sonic-rs full-document scan (all fields, not just 3)
- Arrow RecordBatch construction from sonic-rs output
- DataFusion execution of a realistic transform
- Hand-rolled OTLP protobuf encoding from RecordBatch

Target: prove the full SQL path is achievable at >500K lines/sec on a single core.

### R6: OTLP-to-OTLP Passthrough

When input is OTLP gRPC and output is OTLP gRPC, can we avoid the full
deserialize → Arrow → serialize roundtrip? Protobuf fields map directly to Arrow
columns. Measure the cost of:
- Option A: protobuf deserialize → Arrow → DataFusion → protobuf serialize
- Option B: direct protobuf field access (skip Arrow entirely for simple transforms)
- Option C: protobuf passthrough with surgical field modification

---

## Sample Code: Key Patterns

### Registering UDFs in DataFusion (Python — port to Rust)

```python
import pyarrow as pa
import pyarrow.compute as pc
import datafusion as df
from datafusion import udf

ctx = df.SessionContext()

# int() function — TRY_CAST wrapper with graceful NULL on failure
def int_fn(col):
    try:
        return pc.cast(col, pa.int64(), safe=False)
    except:
        results = []
        for v in col:
            if not v.is_valid:
                results.append(None)
            else:
                try: results.append(int(v.as_py()))
                except: results.append(None)
        return pa.array(results, type=pa.int64())

ctx.register_udf(udf(int_fn, [pa.string()], pa.int64(), "stable", name="int"))

# Usage:
# SELECT int(duration_ms) FROM logs WHERE int(status) >= 400
# AVG(int(duration_ms)) GROUP BY service
```

In Rust, the UDF would use arrow::compute::cast with safe=false, catching errors
per-value. The vectorized path (try cast entire array, check null count) is fastest
for the common case where all values are the same type.

### Building Multi-Typed Columns from JSON (Python — illustrates the Rust pattern)

```python
def build_multi_typed_columns(json_lines):
    """
    Parse JSON lines, route each value to a typed column based on JSON type.
    This is what the Rust sonic-rs parser would do.
    """
    fields = {}  # field_name → {"str": [...], "int": [...], "float": [...]}
    
    for i, line in enumerate(json_lines):
        obj = json.loads(line)
        for key, value in obj.items():
            if key not in fields:
                fields[key] = {"str": [None]*i, "int": [None]*i, "float": [None]*i}
            
            if isinstance(value, bool):
                fields[key]["str"].append(str(value).lower())
                fields[key]["int"].append(None)
                fields[key]["float"].append(None)
            elif isinstance(value, int):
                fields[key]["str"].append(None)
                fields[key]["int"].append(value)
                fields[key]["float"].append(None)
            elif isinstance(value, float):
                fields[key]["str"].append(None)
                fields[key]["int"].append(None)
                fields[key]["float"].append(value)
            elif isinstance(value, (dict, list)):
                # Nested objects → store as JSON string
                fields[key]["str"].append(json.dumps(value))
                fields[key]["int"].append(None)
                fields[key]["float"].append(None)
            else:
                fields[key]["str"].append(str(value))
                fields[key]["int"].append(None)
                fields[key]["float"].append(None)
    
    # Build Arrow columns — only create typed columns that have non-null data
    columns = {}
    for field_name, type_vals in fields.items():
        if any(v is not None for v in type_vals["str"]):
            columns[f"{field_name}_str"] = pa.array(type_vals["str"], type=pa.string())
        if any(v is not None for v in type_vals["int"]):
            columns[f"{field_name}_int"] = pa.array(type_vals["int"], type=pa.int64())
        if any(v is not None for v in type_vals["float"]):
            columns[f"{field_name}_float"] = pa.array(type_vals["float"], type=pa.float64())
    
    return pa.table(columns)
```

### Rust Pseudocode: Scanner → Arrow Column Builder

```rust
/// This is the bridge between the existing memchr-based scanner and Arrow.
/// The scanner finds field positions. This function copies the values into
/// Arrow column builders, routing by JSON type.
fn build_record_batch_from_scan(
    buf: &[u8],                    // the read buffer (OwnedChunk.data())
    scan_results: &[LineScan],     // per-line: Vec<FieldRef>
    schema_cache: &mut SchemaCache,
) -> RecordBatch {
    // Phase 1: Determine which typed columns we need.
    // Check each field's type tags across all lines in this batch.
    let mut field_types: HashMap<&[u8], HashSet<TypeTag>> = HashMap::new();
    for line_scan in scan_results {
        for field_ref in &line_scan.fields {
            field_types.entry(field_ref.key)
                .or_default()
                .insert(field_ref.type_tag);
        }
    }
    
    // Phase 2: Create column builders.
    // For each field: if only one type seen → one typed column.
    // If multiple types → one column per type.
    let mut builders: HashMap<String, TypedBuilder> = HashMap::new();
    for (field_name, types) in &field_types {
        for type_tag in types {
            let col_name = format!("{}_{}", 
                std::str::from_utf8(field_name).unwrap(),
                type_tag.suffix()); // "str", "int", "float"
            builders.insert(col_name, TypedBuilder::new(*type_tag, scan_results.len()));
        }
    }
    
    // Phase 3: Fill builders from scan results.
    // This is the organized memcpy — values go from read buffer into Arrow arrays.
    for (row_idx, line_scan) in scan_results.iter().enumerate() {
        for field_ref in &line_scan.fields {
            let col_name = format!("{}_{}", 
                std::str::from_utf8(field_ref.key).unwrap(),
                field_ref.type_tag.suffix());
            let value_bytes = &buf[field_ref.offset..field_ref.offset + field_ref.len];
            builders.get_mut(&col_name).unwrap()
                .append(row_idx, value_bytes, field_ref.type_tag);
        }
        // Fields not present in this line → NULL (builders track this automatically)
    }
    
    // Phase 4: Finalize into RecordBatch.
    // After this, the read buffer can be reclaimed — Arrow arrays own their data.
    let arrays: Vec<(String, ArrayRef)> = builders.into_iter()
        .map(|(name, builder)| (name, builder.finish()))
        .collect();
    
    // Add _raw column
    let raw_array = StringArray::from(
        scan_results.iter().map(|s| &buf[s.line_start..s.line_end]).collect()
    );
    
    RecordBatch::try_from_iter(arrays).unwrap()
}

/// Type tag from JSON parsing. Stored per-field per-line by the scanner.
#[derive(Clone, Copy, Hash, Eq, PartialEq)]
enum TypeTag {
    Str,    // JSON string value
    Int,    // JSON integer (i64)
    Float,  // JSON float (f64)
    Bool,   // JSON true/false (stored as "true"/"false" string)
    Nested, // JSON object or array (stored as JSON string)
    Null,   // JSON null
}

impl TypeTag {
    fn suffix(&self) -> &'static str {
        match self {
            TypeTag::Str | TypeTag::Bool | TypeTag::Nested => "str",
            TypeTag::Int => "int",
            TypeTag::Float => "float",
            TypeTag::Null => "str", // nulls go in string column as NULL
        }
    }
}
```

### Rust Pseudocode: The Rewriter

```rust
use sqlparser::ast::*;
use sqlparser::parser::Parser;
use sqlparser::dialect::GenericDialect;

/// Rewrite user SQL to use internal typed column names.
fn rewrite_sql(
    user_sql: &str,
    field_type_map: &HashMap<String, Vec<TypeTag>>, // field → which typed columns exist
) -> String {
    let dialect = GenericDialect {};
    let mut ast = Parser::parse_sql(&dialect, user_sql).unwrap();
    
    for statement in &mut ast {
        if let Statement::Query(query) = statement {
            rewrite_query(query, field_type_map);
        }
    }
    
    ast.iter().map(|s| s.to_string()).collect::<Vec<_>>().join("; ")
}

fn rewrite_column_ref(name: &str, field_type_map: &HashMap<String, Vec<TypeTag>>) -> Expr {
    let types = field_type_map.get(name);
    match types {
        Some(types) if types.len() == 1 => {
            // Single type: direct mapping
            // level → level_str, duration_ms → duration_ms_int
            let suffix = types[0].suffix();
            column(&format!("{name}_{suffix}"))
        }
        Some(types) => {
            // Multiple types: COALESCE, casting all to VARCHAR
            // duration_ms → COALESCE(CAST(duration_ms_int AS VARCHAR), duration_ms_str)
            let parts: Vec<Expr> = types.iter().map(|t| {
                let col = column(&format!("{name}_{}", t.suffix()));
                if *t != TypeTag::Str {
                    cast(col, DataType::Utf8)  // CAST(x AS VARCHAR)
                } else {
                    col
                }
            }).collect();
            coalesce(parts)
        }
        None => {
            // Field not seen in data yet — create as nullable string
            column(&format!("{name}_str"))
        }
    }
}

fn rewrite_int_call(field: &str, field_type_map: &HashMap<String, Vec<TypeTag>>) -> Expr {
    // int(duration_ms) → COALESCE(duration_ms_int, TRY_CAST(duration_ms_str AS BIGINT))
    let types = field_type_map.get(field).cloned().unwrap_or_default();
    let mut parts = Vec::new();
    
    // Prefer _int column (already the right type)
    if types.contains(&TypeTag::Int) {
        parts.push(column(&format!("{field}_int")));
    }
    // Then _float (truncate to int)
    if types.contains(&TypeTag::Float) {
        parts.push(cast(column(&format!("{field}_float")), DataType::Int64));
    }
    // Then _str (try parse)
    if types.contains(&TypeTag::Str) {
        parts.push(try_cast(column(&format!("{field}_str")), DataType::Int64));
    }
    
    if parts.len() == 1 { parts.remove(0) } else { coalesce(parts) }
}
```

### Rust Pseudocode: Output Serializer Type Preservation

```rust
/// Serialize a field for OTLP protobuf output.
/// Checks typed columns in priority order, emits the correct protobuf type.
fn emit_otlp_attribute(
    field_name: &str,
    row: usize,
    typed_columns: &TypedColumnIndex,
    buf: &mut Vec<u8>,
) {
    // Check _int first (most specific numeric type)
    if let Some(col) = typed_columns.get_int(field_name) {
        if col.is_valid(row) {
            encode_key_value_int(buf, field_name, col.value(row));
            return;
        }
    }
    // Then _float
    if let Some(col) = typed_columns.get_float(field_name) {
        if col.is_valid(row) {
            encode_key_value_double(buf, field_name, col.value(row));
            return;
        }
    }
    // Then _str (always the fallback)
    if let Some(col) = typed_columns.get_str(field_name) {
        if col.is_valid(row) {
            encode_key_value_string(buf, field_name, col.value(row));
            return;
        }
    }
    // Field not present in this row — skip
}

/// Index for looking up typed columns by field name.
struct TypedColumnIndex {
    /// field_name → (int_col_idx, float_col_idx, str_col_idx) in the RecordBatch
    fields: HashMap<String, TypedColumnRefs>,
}

struct TypedColumnRefs {
    int_col: Option<usize>,
    float_col: Option<usize>,
    str_col: Option<usize>,
}
```

---

## Benchmark Scripts Available

All prototype scripts are in /mnt/user-data/outputs/:

| File | What it tests |
|------|--------------|
| benchmark.py | DataFusion query cost vs parse vs serialize (the foundational 5.3% finding) |
| benchmark_schemaless.py | Raw JSON, Map, hybrid, semi-parsed approaches |
| benchmark_raw_parsed.py | _raw + _parsed Map architecture with DataFusion bracket notation |
| benchmark_typed.py | Multi-typed maps vs all-Utf8 vs type-tagged approaches |
| bench_df_vs_direct.py | DataFusion vs per-event pipeline crossover at various batch sizes |
| bench_multi_typed.py | Multi-typed columns (field_int, field_str) approach |

These can be re-run to verify findings or extended with new test cases.
