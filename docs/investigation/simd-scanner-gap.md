# SIMD Scanner Performance Gap Investigation (Issue #23)

Research-only. No code changes.

---

## 1. Current Scanner Architecture

**Files:** `crates/logfwd-core/src/chunk_classify.rs`, `crates/logfwd-core/src/scanner.rs`

The scanner has two stages, explicitly modeled after simdjson's stage 1 / stage 2 split:

### Stage 1: ChunkIndex (chunk_classify.rs)

Processes the entire NDJSON buffer in 64-byte blocks. For each block:

1. Loads 4x16-byte NEON vectors (`vld1q_u8`), compares against `"` and `\` using `vceqq_u8`.
2. Converts NEON comparison results to a 64-bit bitmask via `vpaddq_u8` horizontal reductions (`neon_to_bitmask64`).
3. Computes `real_quotes` by masking out escaped quotes (uses a loop over backslash bits, not the full simdjson prefix_xor trick for escapes).
4. Builds `in_string` mask via `prefix_xor(real_quotes)` -- this IS the simdjson prefix_xor algorithm.

Output: two `Vec<u64>` bitmask arrays (`real_quotes`, `in_string`) covering the entire buffer.

**NEON only.** The `#[cfg(not(target_arch = "aarch64"))]` fallback is byte-at-a-time scalar code that builds equivalent bitmasks but without SIMD.

### Stage 2: scan_into / scan_line (scanner.rs)

A scalar state machine walks top-level JSON objects. Uses ChunkIndex for:
- `scan_string()`: O(1) closing-quote lookup via `next_quote()` (trailing_zeros on pre-computed bitmask).
- `skip_nested()`: walks braces/brackets, checking `is_in_string()` per byte to skip structural characters inside strings.
- `is_in_string()`: O(1) bit lookup into pre-computed mask.

The scan loop is generic over `ScanBuilder` (two implementations: `StorageBuilder` for owned data, `StreamingBuilder` for zero-copy `StringViewArray`).

### What SIMD it uses

- **aarch64**: NEON intrinsics for stage 1 only (quote/backslash detection + bitmask extraction). Stage 2 is scalar.
- **x86_64**: Pure scalar fallback for stage 1. Stage 2 is the same scalar code.
- **memchr** crate used for newline finding (internally SIMD-accelerated on all platforms).
- **sonic-simd** listed as a dependency but not used in the scanner path (likely used elsewhere or vestigial).

---

## 2. Comparison with simdjson

### What simdjson does

simdjson has two stages:
1. **Stage 1 (structural indexing)**: SIMD pass over the entire buffer. Finds all structural characters (`{`, `}`, `[`, `]`, `:`, `,`, `"`) and produces a flat array of their positions (the "structural index"). Also classifies string interiors to distinguish structural characters inside strings from real ones. Uses 256-bit (AVX2) or 128-bit (SSE/NEON) vectors.
2. **Stage 2 (tape construction)**: Walks the structural index to build a "tape" -- a flat array of typed tokens (start object, key, string value, number value, etc.) with offset pointers. This is the full DOM.

### How logfwd compares

logfwd's ChunkIndex IS simdjson-style stage 1, but scoped to what logfwd needs:
- It only indexes **quotes** and **string interiors** (not all structural characters).
- It does NOT produce a flat structural index array. Instead, it provides O(1) bit-lookup primitives (`next_quote`, `is_in_string`) that the scalar stage 2 calls on demand.

logfwd's stage 2 is NOT simdjson-style:
- simdjson walks a pre-computed structural index array sequentially (predictable memory access, no branches on buffer content).
- logfwd walks the raw buffer byte-by-byte for structural characters (`{`, `}`, `:`, `,`), only using ChunkIndex for string scanning.

### The gap

The architectural difference is that logfwd's stage 2 does per-byte branching on `{`, `}`, `:`, `,`, whitespace, while simdjson's stage 2 jumps directly between structural positions. For logfwd's use case (flat top-level JSON objects with string-heavy log data), the per-byte walk is mostly hitting whitespace skip and value classification -- the heavy lifting (string scanning) already uses the pre-computed index.

A full simdjson-style structural index would help most on:
- Deeply nested JSON (many structural characters to skip).
- Wide objects where field pushdown skips many unwanted fields.

It would help less on:
- Narrow objects (5-12 fields) where most values are strings -- string scanning already uses SIMD.
- Log lines where the scan loop is dominated by string content, not structural overhead.

---

## 3. Existing Benchmarks

### Scanner microbenchmarks (crates/logfwd-core/benches/scanner.rs)

Criterion benchmarks comparing three parsers across 13 scenarios:
- **SIMD scanner** (logfwd's custom scanner)
- **sonic-rs DOM** (full SIMD JSON parser, then walk DOM to build Arrow)
- **arrow-json** (spec-compliant Arrow JSON reader, used as slow baseline)

Scenarios cover: narrow (5 fields), wide (55 fields), K8s realistic (12 fields), long messages, sparse fields, nested JSON, escape-heavy strings. Also batch size scaling (100 to 100K rows).

Each scenario measures throughput in bytes/sec.

### Pipeline benchmarks (crates/logfwd-bench/benches/pipeline.rs)

End-to-end benchmarks: scan -> transform -> output. Measures scanner, CRI parsing, DataFusion transform (SELECT *, WHERE filter, regexp_extract, grok), compression, and full pipeline.

### Reported numbers (from docs/research/BENCHMARKS_V1.md, aarch64)

```
Scan-only 3 fields (early exit):        74 ns/line   13.5M lines/sec
Scan-and-build all fields into Arrow:  399 ns/line    2.5M lines/sec
Scan-and-build 3 fields (pushdown):    342 ns/line    2.9M lines/sec
Arrow column construction overhead:     93 ns/line
```

Stage 1 (ChunkIndex) runs at ~16 GiB/s per ARCHITECTURE.md.

### Full pipeline breakdown

```
scan -> DataFusion passthrough: ~500 ns/line -> 2.0M lines/sec
scan -> DataFusion filter:      ~450 ns/line -> 2.2M lines/sec
scan -> DataFusion + OTLP out:  ~700 ns/line -> 1.4M lines/sec
scan -> DataFusion + JSON out:  ~900 ns/line -> 1.1M lines/sec
```

v1 profiling showed: read 6%, CRI 17%, zstd 78% (compression dominates).
Without compression: read 24%, CRI 76%.

No x86_64 benchmark numbers found in the codebase.

---

## 4. x86_64 SIMD Gap

### Current state

- **No AVX2/SSE2 implementation exists.** The `#[cfg(not(target_arch = "aarch64"))]` path in `ChunkIndex::new()` is a byte-at-a-time scalar loop.
- `sonic-simd = "0.1"` is listed as a dependency in Cargo.toml but is not used in the scanner or ChunkIndex code paths on the main branch.
- CI runs on x86_64 Linux (GitHub Actions). The scanner compiles and passes tests but runs at scalar speed for stage 1.

### Impact estimate

Stage 1 on aarch64 runs at ~16 GiB/s. The scalar fallback is likely 2-4 GiB/s (processing one byte at a time with branches). However:

- Stage 1 is a small fraction of total scan time. The v1 benchmarks show 399 ns/line total for scan+build, and the bulk is in stage 2 field extraction and Arrow construction.
- Adding AVX2 intrinsics to `find_quotes_and_backslashes` would be straightforward: `_mm256_cmpeq_epi8` + `_mm256_movemask_epi8` for 32 bytes at a time, or SSE2 for 16 bytes. The bitmask extraction is simpler on x86 than NEON (movemask is a single instruction vs. the vpaddq chain).

### Would adding x86 SIMD close the gap?

Partially. It would bring stage 1 to parity (~16 GiB/s on AVX2). But stage 2 is identical on both platforms (scalar byte walk), so the overall scanner throughput gap between aarch64 and x86_64 is smaller than the stage 1 gap alone. The main benefit would be for very large buffers or escape-heavy data where stage 1 dominates.

An x86 SIMD implementation for stage 1 is ~50-80 lines of code (mirroring the NEON path with x86 intrinsics). Low risk, moderate reward.

---

## 5. simd-json / simdjson-rs Crate Assessment

### Available crates

- **`simd-json`** (crates.io): Rust port of simdjson. Full structural indexing + tape construction. Supports AVX2, SSE4.2, NEON. Mutable input buffer (modifies in-place for string unescaping). ~2.5 GiB/s on AVX2 for full DOM parse.
- **`simdjson-rust`**: Rust bindings to the C++ simdjson library. FFI overhead. Less idiomatic.
- **`sonic-rs`** (already a dev-dependency): SIMD JSON parser by ByteDance. Used in benchmarks as a comparison baseline. Supports AVX2, SSE2, NEON. Does not require mutable input.

### What we would lose by replacing the custom scanner

1. **Field pushdown**: logfwd's scanner skips unwanted fields without parsing their values. A DOM parser (simd-json, sonic-rs) parses everything, then you select. The scanner benchmarks show pushdown is valuable: 342 ns vs 399 ns (14% savings on 3-of-13 fields, more on wider objects).

2. **Type-suffixed columns**: logfwd detects JSON value types during scanning and routes directly to typed Arrow builders (`status_int`, `rate_float`). A DOM parser outputs a generic Value enum that requires a second type-dispatch pass.

3. **Zero-copy streaming**: `StreamingBuilder` records `(offset, len)` into the original buffer. DOM parsers allocate their own string storage. The 20% streaming speedup would be lost.

4. **_raw preservation**: logfwd preserves the original line bytes for passthrough. DOM parsers may normalize/reformat.

5. **No allocation per line**: logfwd's scanner allocates nothing per line -- builders are reused. DOM parsers allocate per-document.

### What we would gain

1. **Full spec compliance**: The custom scanner handles simple JSON well but has edge cases (deeply nested escapes, unicode escapes). DOM parsers are battle-tested.
2. **x86 SIMD for free**: simd-json and sonic-rs include AVX2/SSE2 backends.
3. **Maintenance**: Fewer lines of custom SIMD code to maintain.

### Verdict

Replacing the scanner with simd-json/sonic-rs would sacrifice the key performance features (pushdown, zero-copy, direct-to-Arrow typing). The existing sonic-rs benchmark comparison in `benches/scanner.rs` measures exactly this tradeoff. The custom scanner should be faster for logfwd's use case even if the JSON parsing itself is slightly slower, because it avoids the DOM-to-Arrow conversion step.

---

## 6. Realistic Assessment

### Where time is actually spent

From the v1 profiling and v2 projections:

| Stage | ns/line | % of pipeline |
|-------|---------|--------------|
| Stage 1 (ChunkIndex) | ~5-10 | ~1% |
| Stage 2 (field extraction) | ~230-340 | ~40-50% |
| Arrow construction | ~93 | ~13% |
| DataFusion transform | ~85-500 | ~12-50% |
| OTLP/output encoding | ~200-400 | ~25-40% |

Stage 1 (the SIMD part) is approximately 1% of the pipeline. Even a 10x improvement to stage 1 saves ~5-9 ns/line out of ~500-900 ns total.

Stage 2 (the scalar byte walk) is the largest single component. But it is already efficient: it uses pre-computed bitmasks for the expensive operations (string scanning, nested skipping). The remaining work is `skip_ws`, key lookup, and value type dispatch -- none of which benefit from a structural index.

### Is #23 worth pursuing now?

**No.** Rationale:

1. **Stage 1 is not the bottleneck.** It runs at 16 GiB/s and accounts for ~1% of pipeline time. Making it faster on x86 helps CI benchmarks but not production throughput.

2. **Stage 2 is already simdjson-inspired.** The scanner uses pre-computed bitmasks for the operations that matter (string scanning, nested skipping). The remaining scalar work (whitespace skip, structural character dispatch) is a small fraction.

3. **The real bottlenecks are downstream.** DataFusion transform and output encoding together account for 50-75% of pipeline time. Optimizing those would yield larger gains.

4. **The benchmarks already exist.** The sonic-rs comparison in `benches/scanner.rs` directly measures the gap between logfwd's scanner and a state-of-the-art SIMD JSON parser. Running those benchmarks on the target platform gives the definitive answer.

### What IS worth doing (low-effort, high-signal)

1. **Add x86 SIMD to ChunkIndex** (~50-80 lines): Brings stage 1 to parity on Linux CI. Low risk, eliminates a known gap. Does not require a rewrite.

2. **Run the existing benchmarks on x86_64**: The `benches/scanner.rs` file already compares SIMD scanner vs sonic-rs DOM vs arrow-json. Running this on an x86_64 machine gives concrete numbers for the "performance gap" without any code changes.

3. **Wire StreamingSimdScanner as the default**: Issue triage notes it is "built but not wired." The 20% speedup from zero-copy StringViewArray is a larger win than any stage 1 improvement.

### Decision threshold (from issue-triage.md)

> If <20% gap [vs simdjson-rs], skip. If >2x gap, invest.

Based on architecture analysis, the gap is likely <20% for the full scan-to-Arrow pipeline on aarch64, and the x86 gap can be closed with a targeted 50-80 line SSE2/AVX2 addition to `find_quotes_and_backslashes`. A full simdjson-style rewrite is not warranted.
