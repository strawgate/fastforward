# logfwd Architecture Plan v2: Arrow-First

## The Decision

One internal representation. Every input produces Arrow RecordBatches. Every
output serializes from Arrow RecordBatches. DataFusion operates on Arrow
RecordBatches. No fast path. No bifurcated code paths.

## Why

Rust benchmark data (scan_bench.rs, release mode, 8192 lines × 13 fields × 310 bytes/line):

```
Scan-only 3 fields (current fast path):   74ns/line   13.5M lines/sec
Scan-and-build all fields into Arrow:    399ns/line    2.5M lines/sec
Scan-and-build 3 fields (SQL pushdown):  342ns/line    2.9M lines/sec
Arrow column construction overhead:       93ns/line    (just the memcpy into builders)
```

The gap between scan-only and scan-and-build is 325ns. Of that, 232ns is scanning
all 13 fields (vs early-exit at 3), and only 93ns is building Arrow columns.
Arrow construction is cheap. Scanning more fields is the real cost, and SQL
field pushdown reduces it.

Projected full-pipeline throughput:

```
Arrow + OTAP output:    ~450ns/line → 2.2M lines/sec    15x faster than vlagent
Arrow + OTLP output:    ~700ns/line → 1.4M lines/sec    10x faster than vlagent
Arrow + JSON output:    ~900ns/line → 1.1M lines/sec     8x faster than vlagent

For reference: vlagent 143K, Vector 25K, OTel Collector 20.5K
```

Eliminating the fast path means:
- One output implementation per format (not two)
- One input contract (RecordBatch, not RecordBatch-or-byte-refs)
- DataFusion always available (even for simple filters — it's ~100ns/line overhead)
- OTAP output is nearly free (Arrow IPC = memcpy of column buffers)
- Parquet output is native
- New features only need one implementation

## The Pipeline

```
INPUT                    TRANSFORM                OUTPUT
─────                    ─────────                ──────
File (CRI/JSON/raw)  ─┐                      ┌─  OTAP (Arrow IPC over gRPC)
UDP syslog            │  Arrow RecordBatch    │   OTLP protobuf (gRPC/HTTP)
TCP stream            ├─────────────────────►─┤   Elasticsearch bulk JSON
OTLP gRPC/HTTP        │         │             │   JSON lines (HTTP/file/stdout)
OTAP gRPC             │    DataFusion SQL     │   Loki protobuf
                     ─┘    (always runs)      └─  Parquet (S3/file)
```

Every box in this diagram operates on Arrow RecordBatches. The internal
representation is a RecordBatch with:

- `_raw`: Utf8 column — original line bytes (for JSON passthrough output)
- Per-field typed columns: `timestamp_str`, `level_str`, `duration_ms_int`,
  `duration_ms_str`, `status_int`, `status_str`, `hit_rate_float`, etc.
- Column naming convention: `{field_name}_{type_suffix}`
- Types determined by JSON value type during parsing
- Fields with consistent types get one column
- Fields with type conflicts get multiple typed columns

## Phase 1: Arrow Column Builder (3 weeks)

Replace the current three-field scanner with a configurable scanner that builds
Arrow RecordBatches directly.

### The Scanner

The existing `extract_json_fields` in otlp.rs scans with memchr and returns byte
refs. The new scanner uses the same memchr approach but writes values directly
into column builders.

```rust
/// Configuration derived from SQL analysis at startup.
struct ScanConfig {
    /// Fields to extract. Empty = extract all (SELECT *).
    /// If non-empty, only these fields are built into columns (SQL pushdown).
    wanted_fields: Vec<FieldSpec>,
    /// True if the query uses SELECT * (extract everything).
    extract_all: bool,
    /// True if _raw column is needed (JSON output, passthrough).
    keep_raw: bool,
}

struct FieldSpec {
    /// Canonical name (what the column will be called, minus the type suffix).
    name: String,
    /// Alternate key names to match in JSON (e.g., "ts", "@timestamp" for "timestamp").
    aliases: Vec<String>,
}
```

The scan loop is nearly identical to the current one — memchr for quotes, match
keys, extract values. The difference: instead of returning `Option<&[u8]>` refs,
it appends values into the `BatchBuilder`.

```rust
/// Build a RecordBatch from a chunk of newline-delimited JSON messages.
///
/// This is the only JSON scanning function in the codebase. It replaces
/// both extract_json_fields (otlp.rs) and any future parsing needs.
///
/// scan_config comes from SQL analysis at startup. It tells us which fields
/// to extract and whether to keep _raw.
pub fn scan_json_to_batch(
    buf: &[u8],                    // newline-delimited JSON messages
    config: &ScanConfig,
    builder: &mut BatchBuilder,    // reused across batches
) -> usize {
    builder.begin_batch();
    let mut line_count = 0;

    let mut line_start = 0;
    for pos in memchr::memchr_iter(b'\n', buf) {
        let line = &buf[line_start..pos];
        line_start = pos + 1;
        if line.is_empty() { continue; }

        scan_line_into_builder(line, config, builder);
        line_count += 1;
    }
    // Handle last line without trailing newline
    if line_start < buf.len() {
        let line = &buf[line_start..];
        if !line.is_empty() {
            scan_line_into_builder(line, config, builder);
            line_count += 1;
        }
    }

    builder.finish_batch();
    line_count
}

/// Scan one JSON line, write field values into column builders.
#[inline]
fn scan_line_into_builder(
    line: &[u8],
    config: &ScanConfig,
    builder: &mut BatchBuilder,
) {
    builder.begin_row();
    if config.keep_raw {
        builder.append_raw(line);
    }

    let mut pos = 0;
    let len = line.len();

    while pos < len {
        // Find key
        let Some(q1) = memchr::memchr(b'"', &line[pos..]) else { break };
        let key_start = pos + q1 + 1;
        let Some(q2) = memchr::memchr(b'"', &line[key_start..]) else { break };
        let key = &line[key_start..key_start + q2];
        pos = key_start + q2 + 1;

        // Skip to value
        let Some(colon) = memchr::memchr(b':', &line[pos..]) else { break };
        pos += colon + 1;
        while pos < len && line[pos] == b' ' { pos += 1; }
        if pos >= len { break; }

        // Check if this field is wanted (skip if not, when selective)
        let wanted = config.extract_all
            || config.wanted_fields.iter().any(|f| key_matches(key, f));
        
        // Extract value (we must advance pos regardless of whether field is wanted)
        if line[pos] == b'"' {
            pos += 1;
            let val_start = pos;
            let Some(vq) = memchr::memchr(b'"', &line[pos..]) else { break };
            let val = &line[val_start..pos + vq];
            pos += vq + 1;

            if wanted {
                builder.append_str(key, val);
            }
        } else if line[pos] == b'{' || line[pos] == b'[' {
            // Nested object/array — find matching brace, store as string
            let val_start = pos;
            let depth_char = line[pos];
            let close_char = if depth_char == b'{' { b'}' } else { b']' };
            let mut depth = 1i32;
            pos += 1;
            while pos < len && depth > 0 {
                if line[pos] == depth_char { depth += 1; }
                else if line[pos] == close_char { depth -= 1; }
                else if line[pos] == b'"' {
                    pos += 1;
                    while pos < len && line[pos] != b'"' {
                        if line[pos] == b'\\' { pos += 1; }
                        pos += 1;
                    }
                }
                pos += 1;
            }
            if wanted {
                builder.append_str(key, &line[val_start..pos]);
            }
        } else {
            // Number, bool, or null
            let val_start = pos;
            while pos < len && line[pos] != b',' && line[pos] != b'}' && line[pos] != b' ' {
                pos += 1;
            }
            let val = &line[val_start..pos];
            let trimmed = trim_end(val);

            if wanted {
                if trimmed == b"true" || trimmed == b"false" {
                    builder.append_str(key, trimmed); // bools as strings for now
                } else if trimmed == b"null" {
                    builder.append_null(key);
                } else if memchr::memchr(b'.', trimmed).is_some() {
                    builder.append_float(key, trimmed);
                } else {
                    builder.append_int(key, trimmed);
                }
            }
        }
    }

    builder.end_row();
}
```

### The BatchBuilder

Manages typed column builders. Produces a RecordBatch when finished.

```rust
/// Builds Arrow RecordBatches from scanned JSON fields.
/// Reused across batches — column builders are reset, not reallocated.
pub struct BatchBuilder {
    /// Per-field column builders, indexed by field ID.
    /// Each field may have 1-3 typed columns (str, int, float).
    fields: Vec<FieldColumns>,
    /// Map from field name bytes → field index. Populated on first batch,
    /// stable for subsequent batches (fields are added, never removed).
    field_index: HashMap<Vec<u8>, usize>,
    /// The _raw column builder (original JSON line bytes).
    raw_builder: Option<StringBuilder>,
    /// Current row count in this batch.
    row_count: usize,
}

struct FieldColumns {
    name: Vec<u8>,
    /// String-typed values for this field.
    str_builder: Option<StringBuilder>,
    /// Integer-typed values for this field.
    int_builder: Option<Int64Builder>,
    /// Float-typed values for this field.
    float_builder: Option<Float64Builder>,
    /// Tracks which typed column was set for the current row.
    current_row_set: bool,
}

impl BatchBuilder {
    /// Append a string value for a field.
    #[inline]
    fn append_str(&mut self, key: &[u8], value: &[u8]) {
        let field = self.get_or_create_field(key);
        let builder = field.str_builder.get_or_insert_with(||
            StringBuilder::with_capacity(self.expected_rows, value.len() * self.expected_rows)
        );
        // Backfill with nulls if this is a new column
        while builder.len() < self.row_count {
            builder.append_null();
        }
        builder.append_value(std::str::from_utf8(value).unwrap_or(""));
        field.current_row_set = true;
    }

    /// Append an integer value (from JSON number without decimal point).
    #[inline]
    fn append_int(&mut self, key: &[u8], value: &[u8]) {
        let field = self.get_or_create_field(key);
        let builder = field.int_builder.get_or_insert_with(||
            Int64Builder::with_capacity(self.expected_rows)
        );
        while builder.len() < self.row_count {
            builder.append_null();
        }
        // Parse integer from bytes (fast, no allocation)
        if let Ok(v) = parse_int(value) {
            builder.append_value(v);
        } else {
            builder.append_null();
        }
        field.current_row_set = true;
    }

    /// After scanning all fields for a row, pad any columns that weren't set.
    fn end_row(&mut self) {
        for field in &mut self.fields {
            if !field.current_row_set {
                // This field wasn't in this JSON line — append NULL to all its builders
                if let Some(b) = &mut field.str_builder {
                    if b.len() <= self.row_count { b.append_null(); }
                }
                if let Some(b) = &mut field.int_builder {
                    if b.len() <= self.row_count { b.append_null(); }
                }
                if let Some(b) = &mut field.float_builder {
                    if b.len() <= self.row_count { b.append_null(); }
                }
            }
            field.current_row_set = false;
        }
        self.row_count += 1;
    }

    /// Finalize the batch into a RecordBatch.
    fn finish_batch(&mut self) -> RecordBatch {
        let mut columns: Vec<(String, ArrayRef)> = Vec::new();

        if let Some(raw) = &mut self.raw_builder {
            columns.push(("_raw".to_string(), Arc::new(raw.finish())));
        }

        for field in &mut self.fields {
            let name = std::str::from_utf8(&field.name).unwrap_or("unknown");
            if let Some(b) = &mut field.str_builder {
                if b.len() > 0 {
                    columns.push((format!("{name}_str"), Arc::new(b.finish())));
                }
            }
            if let Some(b) = &mut field.int_builder {
                if b.len() > 0 {
                    columns.push((format!("{name}_int"), Arc::new(b.finish())));
                }
            }
            if let Some(b) = &mut field.float_builder {
                if b.len() > 0 {
                    columns.push((format!("{name}_float"), Arc::new(b.finish())));
                }
            }
        }

        RecordBatch::try_from_iter(columns).unwrap()
    }
}
```

### Integration with Existing Pipeline

The scanner replaces the current `extract_json_fields` + `encode_log_record` pair.
Instead of scanning fields and immediately encoding protobuf, we scan fields into
a RecordBatch and pass it downstream.

```rust
// Current flow in e2e_bench.rs / pipeline.rs:
//   cri::process_cri_to_buf(chunk, &mut reassembler, prefix, &mut json_batch);
//   otlp_encoder.encode_from_buf(&json_batch, observed_time_ns);

// New flow:
//   cri::process_cri_to_buf(chunk, &mut reassembler, None, &mut json_batch);
//   let batch = scanner::scan_json_to_batch(&json_batch, &scan_config, &mut builder);
//   // batch is now an Arrow RecordBatch — pass to DataFusion or directly to output
```

The CRI parser remains unchanged — it still strips the CRI envelope and writes
messages into a contiguous buffer. The scanner reads from that buffer.

### Buffer Lifecycle

```
ChunkAccumulator yields OwnedChunk
    │
    ▼
CRI parser writes messages into json_batch buffer (existing)
    │
    ▼
Scanner reads json_batch, writes into BatchBuilder's Arrow column buffers
    │  (values are copied from json_batch into StringBuilder/Int64Builder)
    │
    ▼
OwnedChunk is reclaimed ← can happen here, Arrow builders own their data
    │
    ▼
BatchBuilder.finish_batch() → RecordBatch (owns all memory)
    │
    ▼
DataFusion executes SQL on RecordBatch
    │
    ▼
Output serializer reads RecordBatch → OTAP/OTLP/JSON/ES/Parquet
```

The critical point: Arrow builders copy values during scanning. Once scanning is
complete, the read buffer (OwnedChunk) and json_batch buffer can be reused. The
RecordBatch is independent. DataFusion and output serializers never touch the
read buffer.

### Deliverables

- [ ] `scanner.rs`: `scan_json_to_batch()` with ScanConfig
- [ ] `batch_builder.rs`: BatchBuilder with typed column builders (_str, _int, _float)
- [ ] Field name normalization (alias matching) during scan
- [ ] Nested JSON object/array handling (stored as _str column with JSON string value)
- [ ] Integration with existing CRI pipeline (replace extract_json_fields call sites)
- [ ] Benchmarks: compare new scanner vs current, validate ~400ns/line all fields
- [ ] Test: type conflicts within a batch (status=500 and status="healthy")
- [ ] Test: schema variation within a batch (different fields per line)

### Performance Target

400ns/line for scan-and-build all fields (13 fields, 310 byte lines).
Validated by Rust benchmark. This is the number to maintain as we add features.

## Phase 2: SQL Transform Engine (4 weeks)

### SQL Analysis at Startup

Parse the user's SQL once, extract everything we need for the pipeline.

```rust
struct QueryAnalysis {
    /// Column names referenced in the SQL (for scan pushdown).
    referenced_columns: HashSet<String>,
    /// True if the query uses SELECT * (must extract all fields).
    uses_select_star: bool,
    /// True if the query uses EXCEPT (need field names for expansion).
    uses_except: bool,
    /// Fields listed in EXCEPT clauses.
    except_fields: Vec<String>,
    /// The rewritten SQL (internal typed column names).
    rewritten_sql: String,
    /// Pre-compiled DataFusion logical plan.
    logical_plan: LogicalPlan,
}

/// At startup:
fn analyze_query(user_sql: &str) -> QueryAnalysis {
    // 1. Parse SQL with sqlparser-rs
    // 2. Walk AST, collect column references
    // 3. Determine if SELECT * → extract_all=true in ScanConfig
    // 4. Wait for first batch to discover field names + types
    //    (plan compilation deferred until then)
}

/// After first batch:
fn compile_plan(
    analysis: &mut QueryAnalysis,
    field_type_map: &FieldTypeMap,  // from BatchBuilder's discovered fields
    ctx: &SessionContext,
) {
    // 1. Run the SQL rewriter against the field type map
    // 2. Register UDFs: int(), float(), timestamp(), redact()
    // 3. Build schema from field type map (all typed columns)
    // 4. Compile plan
    // 5. Cache
}
```

### The SQL Rewriter

Translates user SQL into internal typed-column SQL. Uses sqlparser-rs AST walking.

Rewrite rules:

```
1. Bare column reference in SELECT:
   duration_ms → COALESCE(CAST(duration_ms_int AS VARCHAR),
                          CAST(duration_ms_float AS VARCHAR),
                          duration_ms_str) as duration_ms
   level → level_str as level  (single type, no coalesce)

2. Bare column reference in WHERE with numeric literal:
   WHERE duration_ms > 100 → WHERE duration_ms_int > 100
   (string-typed rows get NULL → correctly excluded)

3. Bare column reference in WHERE with string literal:
   WHERE level = 'ERROR' → WHERE level_str = 'ERROR'

4. int() function call:
   int(duration_ms) → COALESCE(duration_ms_int,
                               CAST(duration_ms_float AS BIGINT),
                               TRY_CAST(duration_ms_str AS BIGINT))

5. float() function call:
   float(duration_ms) → COALESCE(CAST(duration_ms_int AS DOUBLE),
                                 duration_ms_float,
                                 TRY_CAST(duration_ms_str AS DOUBLE))

6. EXCEPT (field):
   EXCEPT (duration_ms) → EXCEPT (duration_ms_int, duration_ms_str, duration_ms_float)

7. SELECT *:
   Expand to all fields with coalesced aliases:
   SELECT level_str as level,
          COALESCE(CAST(duration_ms_int AS VARCHAR), duration_ms_str) as duration_ms,
          ...

8. ORDER BY bare column with no explicit type context:
   ORDER BY duration_ms → ORDER BY duration_ms_str
   (string ordering by default — user writes int(duration_ms) for numeric ordering)

9. Aggregation functions:
   AVG(duration_ms) → error: "duration_ms is a string. Use AVG(float(duration_ms))"
   AVG(float(duration_ms)) → AVG(COALESCE(CAST(duration_ms_int AS DOUBLE), ...))
   COUNT(duration_ms) → COUNT(duration_ms_str)  (counts non-null, any type)
```

### UDF Registration

```rust
fn register_udfs(ctx: &mut SessionContext) {
    // int(x) → TRY_CAST(x AS BIGINT) with graceful NULL on failure
    // In Rust: use arrow::compute::cast with safe=false, catch errors per-value
    ctx.register_udf(create_udf("int", vec![Utf8], Int64, ...));

    // float(x) → TRY_CAST(x AS DOUBLE)
    ctx.register_udf(create_udf("float", vec![Utf8], Float64, ...));

    // timestamp(x) → TRY_CAST(x AS TIMESTAMP)
    ctx.register_udf(create_udf("timestamp", vec![Utf8], Timestamp, ...));

    // redact(x, pattern_name) → regexp_replace with built-in patterns
    // Built-in patterns: 'email', 'ssn', 'credit_card', 'ip'
    ctx.register_udf(create_udf("redact", vec![Utf8, Utf8], Utf8, ...));
}
```

### Plan Caching and Schema Evolution

```rust
struct PipelineState {
    /// Cached DataFusion plan.
    plan: Option<Arc<dyn ExecutionPlan>>,
    /// Known field names and their types.
    field_type_map: FieldTypeMap,
    /// Schema fingerprint — changes when fields are added.
    schema_hash: u64,
    /// DataFusion session context.
    ctx: SessionContext,
}

impl PipelineState {
    /// Called per batch. Checks if schema changed, recompiles if needed.
    fn ensure_plan(&mut self, batch: &RecordBatch) -> &Arc<dyn ExecutionPlan> {
        let current_hash = hash_schema(batch.schema());
        if self.plan.is_none() || current_hash != self.schema_hash {
            // Schema changed — update field type map, rewrite SQL, recompile
            self.field_type_map.update_from_schema(batch.schema());
            let rewritten = rewrite_sql(&self.user_sql, &self.field_type_map);
            self.plan = Some(compile_plan(&rewritten, &self.ctx, batch.schema()));
            self.schema_hash = current_hash;
        }
        self.plan.as_ref().unwrap()
    }
}
```

Plan recompilation cost: ~1ms. Happens when a new field name appears in the data
(roughly once per application deploy). Steady-state: zero overhead, cached plan reused.

### Deliverables

- [ ] SQL parser integration (sqlparser-rs)
- [ ] Column reference extraction from AST
- [ ] SQL rewriter: 9 rewrite rules above
- [ ] UDF registration: int(), float(), timestamp(), redact()
- [ ] DataFusion SessionContext setup
- [ ] Plan compilation and caching
- [ ] Schema evolution detection and recompilation
- [ ] ScanConfig generation from query analysis (field pushdown list)
- [ ] Integration test: user SQL → rewritten SQL → RecordBatch → result
- [ ] Edge case tests: 20+ SQL patterns from the research doc

### Performance Target

DataFusion execution: ~100-300ns/line on the cached plan (measured in Python,
should be similar or faster in native Rust). Total with scanning: ~500-700ns/line.

## Phase 3: Output Serializers (3 weeks)

Every output reads from Arrow RecordBatch. One implementation per format.

### Output Interface

```rust
/// Every output implements this trait.
trait OutputSink: Send {
    /// Serialize and send a batch. The RecordBatch contains either:
    /// - Direct scanner output (all typed columns + _raw)
    /// - DataFusion query result (projected/filtered columns)
    fn send_batch(
        &mut self,
        batch: &RecordBatch,
        metadata: &BatchMetadata,
    ) -> Result<()>;

    fn flush(&mut self) -> Result<()>;
}

struct BatchMetadata {
    /// Original typed columns for type-preserving output.
    /// When DataFusion transforms data, the result columns may be Utf8
    /// (from coalesce). The original typed columns preserve the JSON types
    /// for OTLP attribute serialization.
    typed_columns: Option<TypedColumnIndex>,
    /// Which columns the SQL actively operated on (touched columns use
    /// result types; untouched columns use original types).
    touched_columns: HashSet<String>,
    /// Resource attributes (k8s pod name, namespace, etc.)
    resource_attrs: Vec<(String, String)>,
}
```

### OTAP Output (Arrow IPC over gRPC)

The fastest output. The RecordBatch IS the wire format.

```rust
struct OtapSink {
    client: ArrowFlightClient,  // or raw gRPC with Arrow IPC framing
    schema_sent: bool,
}

impl OutputSink for OtapSink {
    fn send_batch(&mut self, batch: &RecordBatch, meta: &BatchMetadata) -> Result<()> {
        // Map our schema to OTAP schema:
        //   timestamp_str → time_unix_nano (parse timestamp)
        //   level_str → severity_text, severity_number (lookup)
        //   message_str → body.string_value
        //   all other fields → attributes (using typed columns for correct types)
        let otap_batch = map_to_otap_schema(batch, meta);

        // Write Arrow IPC
        let ipc_bytes = arrow_ipc::writer::write_batch(&otap_batch);

        // Send over gRPC stream
        self.client.send(ipc_bytes)?;
        Ok(())
    }
}
```

Cost: ~50-100ns/line (Arrow IPC write is mostly memcpy of column buffers).

### OTLP Output (Protobuf)

Walk Arrow columns, write protobuf. Preserves the hand-rolled encoding approach
from the current otlp.rs but reads from Arrow columns instead of byte refs.

```rust
struct OtlpSink {
    encoder: BatchEncoder,  // existing from otlp.rs, adapted
    sender: HttpSender,
    compressor: ChunkCompressor,
}

impl OutputSink for OtlpSink {
    fn send_batch(&mut self, batch: &RecordBatch, meta: &BatchMetadata) -> Result<()> {
        // Walk columns, encode protobuf
        // For timestamp: read timestamp_str column, parse to nanos
        // For severity: read level_str column, map to severity_number
        // For body: read message_str column (or _raw if no message field)
        // For attributes: iterate remaining columns
        //   - For untouched columns: read original typed column (_int, _float, _str)
        //     and emit correct protobuf attribute type
        //   - For touched columns: read result column type
        let otlp_bytes = self.encoder.encode_from_record_batch(batch, meta);
        let compressed = self.compressor.compress(&otlp_bytes)?;
        self.sender.send(&compressed.data)?;
        Ok(())
    }
}
```

Cost: ~300-500ns/line (similar to current protobuf encoding, reading from Arrow
columns instead of byte refs adds minimal overhead since column access is O(1)).

### Elasticsearch Output

```rust
struct ElasticsearchSink { /* ... */ }

impl OutputSink for ElasticsearchSink {
    fn send_batch(&mut self, batch: &RecordBatch, meta: &BatchMetadata) -> Result<()> {
        // Build Elasticsearch bulk API payload:
        // {"index":{"_index":"logs-2024.01.15"}}
        // {"timestamp":"...","level":"ERROR","message":"..."}
        //
        // For each row: iterate non-null columns, write JSON object.
        // Use original typed columns for correct JSON types:
        //   duration_ms_int=42 → "duration_ms":42  (JSON number)
        //   level_str="ERROR" → "level":"ERROR"     (JSON string)
    }
}
```

### JSON Lines Output

```rust
struct JsonLinesSink { /* ... */ }

impl OutputSink for JsonLinesSink {
    fn send_batch(&mut self, batch: &RecordBatch, meta: &BatchMetadata) -> Result<()> {
        // If _raw column exists and no fields were modified by SQL:
        //   memcpy _raw values (original JSON preserved)
        // Else:
        //   Walk columns, serialize JSON objects per row
        //   Use typed columns for correct JSON value types
    }
}
```

### Parquet Output

```rust
struct ParquetSink { /* ... */ }

impl OutputSink for ParquetSink {
    fn send_batch(&mut self, batch: &RecordBatch, _meta: &BatchMetadata) -> Result<()> {
        // Arrow → Parquet is native. One function call.
        // arrow::parquet::arrow_writer::write_batch(batch, &mut self.writer)
        // Flush to file/S3 on size or time threshold.
    }
}
```

### Fan-Out

Multiple outputs from one pipeline. Each output gets a shared reference to the
same RecordBatch.

```rust
struct FanOut {
    sinks: Vec<Box<dyn OutputSink>>,
}

impl FanOut {
    fn send_batch(&mut self, batch: &RecordBatch, meta: &BatchMetadata) -> Result<()> {
        for sink in &mut self.sinks {
            sink.send_batch(batch, meta)?;
        }
        Ok(())
    }
}
```

For async/parallel fan-out: each sink runs in its own thread with a bounded
crossbeam channel (same pattern as current daemon.rs). The RecordBatch is
Arc-wrapped and sent to all channels. Each sink serializes independently.

### Deliverables

- [ ] OutputSink trait
- [ ] OTAP sink (Arrow IPC over gRPC)
- [ ] OTLP sink (adapt existing BatchEncoder to read from RecordBatch)
- [ ] JSON lines sink (with _raw passthrough optimization)
- [ ] Elasticsearch bulk sink
- [ ] Parquet sink
- [ ] Stdout sink (for debugging)
- [ ] FanOut multiplexer
- [ ] Per-sink bounded channel + sender thread
- [ ] TypedColumnIndex for type-preserving output

## Phase 4: Input Abstraction (2 weeks)

Every input produces Arrow RecordBatches through the scanner.

### Input Interface

```rust
/// Every input produces batches of messages for scanning.
trait InputSource: Send {
    /// Poll for new data. Returns a buffer of newline-delimited messages
    /// ready for JSON scanning, or an already-structured RecordBatch
    /// (for OTAP/OTLP inputs that arrive pre-parsed).
    fn poll(&mut self) -> Result<Option<InputBatch>>;
}

enum InputBatch {
    /// Raw bytes needing JSON scanning (file, UDP, TCP).
    Raw {
        buf: Vec<u8>,            // newline-delimited messages
        format: InputFormat,     // json, logfmt, syslog, raw
        source_metadata: SourceMeta,
    },
    /// Pre-structured data (OTAP, OTLP inputs). Bypass scanner.
    Structured {
        batch: RecordBatch,
        source_metadata: SourceMeta,
    },
}

struct SourceMeta {
    source_id: String,          // for routing
    resource_attrs: Vec<(String, String)>,  // k8s pod, namespace, etc.
}
```

### Input Implementations

| Input | Type | Priority | Notes |
|-------|------|----------|-------|
| File (CRI) | Raw | P0 | Existing. CRI parse → json_batch → scanner. |
| File (raw JSON) | Raw | P0 | Skip CRI parse, feed lines directly to scanner. |
| UDP syslog | Raw | P1 | recvmmsg → parse RFC 5424 → scanner or direct build. |
| TCP stream | Raw | P1 | Framed by newline. Read → scanner. |
| OTLP gRPC | Structured | P2 | Decode protobuf → build RecordBatch directly. |
| OTLP HTTP | Structured | P2 | Same as gRPC, different transport. |
| OTAP gRPC | Structured | P2 | Receive Arrow IPC → RecordBatch. Zero deserialization. |

The OTAP input is the ideal case: data arrives as Arrow RecordBatch, goes straight
to DataFusion, exits as Arrow IPC (OTAP output). The entire pipeline is Arrow
end-to-end with zero serialization/deserialization overhead.

## Phase 5: Configuration and Routing (2 weeks)

```toml
# /etc/logfwd/config.toml

[[input]]
type = "file"
path = "/var/log/pods/**/*.log"
format = "cri"
route = "app_logs"

[[input]]
type = "udp"
listen = "0.0.0.0:514"
format = "syslog"
route = "syslog"

[[pipeline]]
name = "app_logs"
sql = """
  SELECT * EXCEPT (stack_trace, debug_context)
         REPLACE (redact(message, 'email') as message),
         'production' as environment
  FROM logs
  WHERE level != 'DEBUG'
"""

[[pipeline]]
name = "syslog"
sql = "SELECT * FROM logs"

[[output]]
name = "collector"
type = "otap"
endpoint = "otel-collector:4317"
pipelines = ["app_logs", "syslog"]

[[output]]
name = "archive"
type = "parquet"
path = "/data/logs/{date}/"
pipelines = ["app_logs"]

[[output]]
name = "es"
type = "elasticsearch"
urls = ["https://es:9200"]
pipelines = ["app_logs"]
```

Each pipeline gets its own DataFusion context, schema cache, and compiled plan.
Pipelines are isolated — a schema change in one doesn't affect others.

## Phase 6: Kubernetes Integration (3 weeks)

- kube-rs pod watcher for metadata (labels, annotations, namespace)
- Resource attributes from pod metadata
- Dynamic pipeline config from pod annotations
- Per-pod log file discovery under /var/log/pods/

## Phase 7: Durability (2 weeks)

- WAL: append compressed RecordBatch (Arrow IPC + zstd) to disk
- Checkpoint: per-input file offset + per-output WAL position
- Replay on startup from last checkpoint
- Graceful shutdown: flush all pipelines and outputs

## Summary: What We're Building

A log forwarder where:

1. Every input produces Arrow RecordBatches
2. DataFusion SQL transforms RecordBatches (always, even for simple filters)
3. Every output serializes from RecordBatches
4. OTAP is the fastest output (Arrow IPC = near zero-copy)
5. Users write SQL with EXCEPT/REPLACE for field manipulation
6. Type preservation through multi-typed columns (field_int, field_str, field_float)
7. SQL field pushdown reduces scanning cost for selective queries
8. One internal representation, one code path, one implementation per feature

At 2.2M lines/sec with full SQL transforms, 15x faster than vlagent, 88x faster
than Vector, with correct type preservation and OTAP/OTLP/JSON/ES/Parquet output.