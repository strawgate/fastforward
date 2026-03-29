# Investigation: Columnar OTLP Protobuf Encoding (Issue #27)

## Summary

Columnar encoding would provide a **modest 10-20% improvement** in OTLP encoding
throughput, not the 30-50% claimed in the issue. The real bottleneck is the
row-oriented nature of the protobuf output format itself, which forces
per-row assembly regardless of how columns are read. The most impactful
optimization would be **eliminating redundant schema iteration per row** and
**pre-resolving column roles once per batch**.

## 1. Current Encoding Architecture

`OtlpSink::encode_batch()` in `crates/logfwd-output/src/otlp_sink.rs` works
in three phases:

- **Phase 1** (lines 69-76): Loop over rows 0..N, calling
  `encode_row_as_log_record()` for each. Each call scans the entire schema
  (all columns) to identify timestamp/level/message columns, then iterates
  remaining columns for attributes.

- **Phase 2** (lines 78-104): Compute protobuf container sizes bottom-up
  (ScopeLogs, ResourceLogs, ExportLogsServiceRequest).

- **Phase 3** (lines 106-126): Write the final nested protobuf envelope,
  embedding the pre-encoded LogRecords.

The per-row function `encode_row_as_log_record()` (lines 176-308):
1. Scans all schema fields to find special columns (timestamp, level, message)
   -- **this schema scan happens for every single row**, despite being identical.
2. For each identified column, downcasts the Arrow array and reads `row`.
3. Writes protobuf fields: time, severity, body, then iterates all remaining
   columns to emit attributes.

For 8K rows with 10 columns, this means:
- 8K redundant schema scans (field name matching, `parse_column_name()`)
- 80K `array.value(row)` calls, each doing offset arithmetic into the Arrow buffer
- 80K column type dispatch (match on type_suffix)

## 2. Would Column-at-a-Time Help?

**Partially.** Arrow stores each column as a contiguous buffer. Reading
`column[0], column[1], ..., column[N]` sequentially is cache-friendly.
Reading `column0[row], column1[row], column2[row]` across columns for
each row is not inherently cache-hostile for Arrow (each column access
is `base_ptr + offset_array[row]` which is random within that column's
buffer), but it does defeat prefetching when columns are large.

However, the OTLP LogRecord protobuf is inherently row-oriented:

```
ExportLogsServiceRequest {
  resource_logs[] {
    scope_logs[] {
      log_records[] {        // <-- each record is one row
        time_unix_nano
        severity_number
        severity_text
        body { string_value }
        attributes[] { KeyValue { key, value } }
      }
    }
  }
}
```

To produce a LogRecord, we MUST have all field values for that row available
at write time. A pure column-at-a-time approach would need a staging area
for every field of every row, which is essentially what `encode_row_as_log_record`
already does -- just inline.

## 3. What Columnar Pre-Processing Could Actually Do

A hybrid approach that pre-processes columns then assembles per-row:

```
// Once per batch:
let ts_col = find_column("timestamp", schema);
let level_col = find_column("level", schema);
let body_col = find_column("message", schema);
let attr_cols = remaining_columns(schema);

// Pre-extract column arrays (zero-copy Arrow references):
let ts_arr = batch.column(ts_col).as_string::<i32>();
let level_arr = batch.column(level_col).as_string::<i32>();
let body_arr = batch.column(body_col).as_string::<i32>();

// Optional: batch-convert timestamps
let mut ts_nanos = Vec::with_capacity(num_rows);
for row in 0..num_rows {
    ts_nanos.push(parse_timestamp_nanos(ts_arr.value(row).as_bytes()));
}

// Per-row assembly uses pre-resolved references:
for row in 0..num_rows {
    encode_fixed64(buf, 1, ts_nanos[row]);
    encode_severity(buf, level_arr.value(row));
    encode_body(buf, body_arr.value(row));
    for (name, arr) in &attr_cols {
        encode_attribute(buf, name, arr.value(row));
    }
}
```

**What this saves:**
- Eliminates 8K redundant schema scans (N * num_columns string comparisons).
- Eliminates 8K redundant `parse_column_name()` calls per column.
- Column array downcasts happen once instead of N times.
- Potential for batch timestamp parsing optimization (SIMD on the
  contiguous string buffer).

**What this does NOT save:**
- The actual protobuf encoding work (varint encoding, tag writing,
  buffer appending) is identical.
- The per-row `array.value(row)` calls are still needed -- Arrow's
  offset-based string access is already O(1).
- The attribute loop still iterates all attribute columns per row.

## 4. Profiling Data

**No profiling results exist in the repo** for OTLP encoding specifically.

- The benchmark suite (`crates/logfwd-bench/benches/pipeline.rs`) benchmarks
  scanner, CRI parsing, SQL transforms, and compression, but the output
  benchmark uses a `NullSink` that discards data -- it does not measure
  OTLP encoding at all.

- The scanner benchmarks (`crates/logfwd-core/benches/scanner.rs`) compare
  SIMD scanner vs sonic-rs vs arrow-json for JSON parsing, not encoding.

- The competitive benchmark harness (`crates/logfwd-competitive-bench/`)
  supports `--profile` for CPU flamegraphs via `perf record`, but there
  are no saved results in the repo.

- Issue #27 claims "the encoder is currently 60% of total CPU time" but
  cites no measurement. This likely refers to the full output path
  (encode + compress + HTTP send), not just protobuf assembly.

**Recommendation:** Run `cargo bench -p logfwd-bench` with an OtlpSink
benchmark (currently missing) to get real numbers before investing in
optimization.

## 5. Realistic Speedup Estimate

Breaking down the claimed "60% of CPU time" for OTLP encoding:

| Component | Estimated % of Encode Time | Columnar Helps? |
|-----------|---------------------------|-----------------|
| Schema scan per row (find timestamp/level/message cols) | ~15% | YES -- eliminated entirely |
| Column array downcast per access | ~5% | YES -- done once per batch |
| `parse_column_name()` per column per row | ~10% | YES -- done once per batch |
| `parse_timestamp_nanos()` per row | ~10% | Marginal (same work, slightly better locality) |
| `parse_severity()` per row | ~3% | Marginal |
| Protobuf varint/tag encoding | ~25% | NO -- identical work |
| Buffer appending (`extend_from_slice`) | ~20% | NO -- identical work |
| Null checks (`is_null(row)`) | ~5% | Marginal |
| `array.value(row)` offset lookups | ~7% | Marginal (already O(1)) |

**Estimated savings from columnar pre-resolution: ~30% of encode time.**

If encoding is 60% of total CPU, and we save 30% of encoding:
- 0.6 * 0.3 = 18% overall CPU reduction
- That translates to roughly **15-20% throughput improvement**.

Not the 30-50% claimed in the issue. The protobuf encoding itself (varints,
tags, buffer writes) is the actual floor, and columnar access does not help
there.

**The real low-hanging fruit is eliminating the per-row schema scan** (lines
193-239 of `encode_row_as_log_record`). This is pure waste: field roles
are determined by column names in the schema, which are constant across all
rows. Fixing this alone would capture 80% of the available gains with
minimal code change.

## 6. Alternative: Batching and Parallelism

### Smaller batches, more frequent sends
- Current batch size is configurable (4MB / 100ms default).
- Smaller batches reduce per-batch latency but increase per-line overhead
  (more HTTP requests, more protobuf envelope overhead).
- At 8K rows/batch, the protobuf envelope overhead is ~20 bytes -- negligible.
- **Verdict: would increase throughput only if the bottleneck is single-batch
  latency, not encoding CPU.** Not the case here.

### Parallel HTTP connections
- Currently uses synchronous `ureq::post()` (line 161 of `otlp_sink.rs`).
- If the downstream collector is the bottleneck, multiple connections help.
- If CPU encoding is the bottleneck, parallelism requires encoding on
  multiple threads -- which means splitting the RecordBatch into sub-batches.
- Arrow RecordBatch supports zero-copy slicing (`batch.slice(offset, len)`),
  so splitting is cheap.
- **Verdict: more impactful than columnar encoding if the pipeline is
  I/O-bound.** Worth investigating independently.

### Async sends with double-buffering
- Encode batch N while sending batch N-1.
- Requires two encoder buffers and async HTTP (replace `ureq` with
  `reqwest` or `hyper`).
- **Verdict: the highest-impact change for overall throughput.** Encoding
  and network I/O would overlap completely.

## Recommendations

1. **Do first (30 minutes):** Extract column role resolution out of
   `encode_row_as_log_record` into a one-time-per-batch setup step.
   Pre-resolve timestamp/level/message column indices and pre-downcast
   the Arrow arrays. This captures most of the benefit with minimal risk.

2. **Do second (1 day):** Add an OTLP encoding benchmark to
   `crates/logfwd-bench/benches/pipeline.rs` so we can measure before/after.
   The current `NullSink` benchmark tells us nothing about encoding cost.

3. **Skip for now:** Full columnar pre-extraction with staging buffers.
   The additional complexity (pre-allocated per-column staging vectors,
   batch timestamp conversion) buys only ~5% more on top of recommendation 1.

4. **Consider instead:** Async double-buffered sends (encode + send overlap).
   This would likely provide a larger end-to-end throughput gain than any
   encoding optimization.

## Decision

**If <30% speedup on encoding microbenchmark, skip full columnar rewrite**
(per issue-triage.md). The estimated 15-20% overall improvement from
columnar does not clear this bar. The per-batch column resolution fix
(recommendation 1) should be done regardless as a straightforward cleanup.
