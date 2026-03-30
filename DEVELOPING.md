# Developing logfwd

## Workspace layout

```
crates/
  logfwd/              Binary crate. CLI, pipeline orchestration.
  logfwd-core/         Scanner, builders, parsers, diagnostics. The hot path.
  logfwd-config/       YAML config parsing and validation.
  logfwd-transform/    DataFusion SQL transforms, UDFs (grok, regexp_extract).
  logfwd-output/       Output sinks (OTLP, JSON lines, HTTP, stdout).
  logfwd-bench/        Criterion benchmarks for the scanner pipeline.
  logfwd-competitive-bench/  Comparative benchmarks vs other log agents.
```

## Build, test, lint, bench, fuzz

```bash
just test                    # All tests
just lint                    # fmt + clippy + toml + deny + typos
cargo test -p logfwd-core    # Core crate only (fastest iteration)

RUSTFLAGS="-C target-cpu=native" cargo bench --bench scanner -p logfwd-core

cd crates/logfwd-core && cargo +nightly fuzz run scanner -- -max_total_time=300
```

## Speeding up compilation

DataFusion is a large dependency. Several things keep the dev loop fast:

**Profile optimizations (already in `Cargo.toml`):**
- `[profile.dev.package."*"]` and `[profile.test.package."*"]` set `opt-level = 3` for all
  external crates. Heavy deps like DataFusion compile once at opt-level 3 and are cached
  by Cargo — they only recompile when their version changes. Your own workspace code still
  compiles at opt-level 0 (fast). Tests run faster too because the runtime code is optimised.
- `debug = 1` (line-tables-only) in dev and test profiles reduces debug info and speeds up
  linking significantly.

**Faster linker (optional, Linux):**

`lld` can cut linking time by 3–5×. Install and enable:

```bash
# Ubuntu / Debian
sudo apt-get install -y lld
```

Then add a local override to `.cargo/config.toml` (not committed — machine-local):

```toml
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=lld"]
```

**`cargo nextest` (already in justfile):**

```bash
cargo install cargo-nextest
just nextest   # Faster parallel test runner
```

**Target only the crate you changed:**

```bash
cargo test -p logfwd-core     # Skip recompiling the rest of the workspace
cargo test -p logfwd-transform
```

---

## Things that will bite you

Hard-won lessons from building the scanner and builder pipeline.

### The deferred builder pattern exists because incremental null-padding is broken

`StorageBuilder` collects `(row, value)` records during scanning and bulk-builds Arrow columns at `finish_batch`. This seems roundabout — why not write directly to Arrow builders?

Because maintaining column alignment across multiple type builders (str, int, float) per field is a coordination nightmare. When you write an int, you must pad the str and float builders with null. When `end_row` fires, pad all unwritten fields. When a new field appears mid-batch, back-fill all prior rows. We tried this (`IndexedBatchBuilder`); proptest found column length mismatches on multi-line NDJSON with varying field sets.

The deferred pattern is correct by construction: each column is built independently. Gaps are nulls. Columns can never mismatch.

### Chunk-level SIMD classification beats per-line SIMD

We tried three approaches:
1. **Per-line SIMD**: load 16 bytes, compare for `"` and `\`. Slower than scalar on short strings.
2. **sonic-rs DOM**: SIMD JSON parser builds a DOM per line. The DOM allocation is the bottleneck.
3. **Chunk-level classification** (`ChunkIndex`): one NEON/SSE pass over the entire buffer at ~16 GiB/s, pre-computes all quote positions. Then `scan_string` is a single `trailing_zeros` bit-scan.

Approach 3 wins everywhere because classification is amortized across all strings and per-string lookup is O(1).

### The prefix_xor escape detection has a subtle correctness requirement

The simdjson `prefix_xor` algorithm detects escaped quotes by computing backslash-run parity. It works for **consecutive** backslashes (`\\\"` = escaped quote). But for **non-consecutive** backslashes like `\n\"`, `prefix_xor` gives wrong results because it counts ALL backslashes, not per-run.

Our implementation iterates each backslash: mark next byte as escaped, skip escaped backslashes. Fast because most JSON has zero or few backslashes. The carry between 64-byte blocks must be handled.

### The scanner assumes UTF-8 input

`from_utf8_unchecked` throughout the scanner and builders. JSON is UTF-8 by spec, so this holds in practice. But the scanner does NOT validate — non-UTF-8 input is UB. The fuzz target guards against this; production code currently doesn't. See issue #76.

### HashMap field lookup was 60% of total scan time

Profiling showed `get_or_create_field` dominating — SipHash + probe per field per row. Fix: `resolve_field` does the HashMap lookup once per batch. Subsequent rows use the returned index directly. The `ScanBuilder` trait's `resolve_field` + `append_*_by_idx` pattern encodes this.

### StringViewArray memory reporting is misleading

Arrow's `get_array_memory_size()` counts the backing buffer for every column sharing it. If 5 string columns point into the same buffer, reported memory is 5x actual. The `StreamingBuilder` produces shared-buffer columns; memory reports overcount significantly.

### Arrow IPC compression is just a flag

Compressed Arrow IPC is `StreamWriter` with `IpcWriteOptions::try_with_compression(Some(CompressionType::ZSTD))`. Any `RecordBatch` can be compressed. No special builder needed.

### `keep_raw` costs 65% of table memory

The `_raw` column stores the full JSON line. Larger than all other columns combined. Default is `keep_raw: false`.

### CI clippy catches things local clippy misses

Conditional SIMD compilation means dead code warnings differ between aarch64 (macOS) and x86_64 (CI Linux). Always check CI.

### proptest finds bugs unit tests can't

Every time we thought the scanner was correct, proptest broke it. Escapes crossing 64-byte boundaries, fields in different orders, duplicate keys with different types. Run `PROPTEST_CASES=2000` minimum.

Oracle tests compare against sonic-rs as ground truth. Our scanner does first-writer-wins for duplicate keys; sonic-rs does last-writer-wins. Both valid per RFC 8259; oracle tests skip duplicate-key inputs.

### Two builders serve different purposes

- **`StorageBuilder`**: copies values into self-contained Arrow arrays. Input buffer can be freed. For persistence and compression.
- **`StreamingBuilder`**: zero-copy `StringViewArray` views into `bytes::Bytes` buffer. 20% faster. Buffer must stay alive. For real-time query-then-discard.

Both implement `ScanBuilder` trait, sharing the generic `scan_into()` loop.
