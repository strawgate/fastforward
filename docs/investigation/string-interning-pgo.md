# Investigation: String Interning (#25) and PGO (#26)

Research-only. No code changes.

---

## String Interning (#25)

### How `resolve_field()` works today

Both `StorageBuilder` and `StreamingBuilder` use the identical pattern:

```rust
// storage_builder.rs:98-106, streaming_builder.rs:130-138
pub fn resolve_field(&mut self, key: &[u8]) -> usize {
    if let Some(&idx) = self.field_index.get(key) {
        return idx;
    }
    let idx = self.fields.len();
    self.fields.push(FieldCollector::new(key));
    self.field_index.insert(key.to_vec(), idx);
    idx
}
```

- `field_index` is a `HashMap<Vec<u8>, usize>`.
- Every field of every row triggers a `HashMap::get()` call with a `&[u8]` key.
- On miss (new field name), it allocates a `Vec<u8>` and inserts. On hit (common case after first row), it does a hash + probe + compare, no allocation.

### Does `field_index` persist across batches?

**Yes.** `begin_batch()` clears the per-field value collectors (`fc.clear()`) and resets `row_count`, but it does NOT clear `field_index` or `fields`. The HashMap persists across `scan()` calls on the same `SimdScanner` / `StreamingSimdScanner` instance. This means:

- Batch 1 discovers all field names and populates the HashMap.
- Batches 2..N get a fast HashMap hit on every `resolve_field()` call.
- No repeated `Vec<u8>` allocations after discovery.

This is already a form of field index caching.

### Call frequency analysis

The scan loop in `scanner.rs` calls `resolve_field()` once per *wanted* field per row (lines 181, 190, 206, 222, 245). For unwanted fields (pushdown filtering via `config.is_wanted()`), `resolve_field()` is skipped entirely.

For a 4MB batch with typical JSON log lines (~130 bytes each):
- ~30K lines per batch
- ~10 fields per line (common for structured logs)
- = **~300K `resolve_field()` calls per batch**

Each call does:
1. SipHash of the key bytes (typically 3-15 bytes for field names like "host", "status", "timestamp")
2. One or two cache-line reads for the bucket probe
3. Byte comparison on hit

### Cost estimate

- SipHash on a short key: ~15-25ns
- Probe + compare: ~5-10ns
- Total per lookup: ~25-35ns (not 50ns; SipHash is fast for short keys)
- 300K lookups x 30ns = **~9ms per 4MB batch**

On a 100ms batch cycle, that is **~9% of the time budget**. Meaningful but not dominant. The scan loop itself (SIMD JSON parsing, memchr, string extraction) likely dominates.

### Alternatives assessed

**1. `phf` (compile-time perfect hash)**
- Not applicable. Field names are discovered at runtime from log data. `phf` requires compile-time knowledge of the key set.

**2. Linear scan of `Vec<(Vec<u8>, usize)>` for N < 16**
- For 10 fields with ~8 byte keys, a linear scan is 10 x memcmp(8) = ~10-30ns total (cache-hot, sequential).
- This would beat HashMap (~30ns per lookup) for the steady-state case.
- The builder already uses a bitmask (`written_bits: u64`) for dedup on fields 0-63, confirming the design assumes < 64 fields.
- **Recommendation: worth benchmarking.** A `SmallVec<[(Vec<u8>, usize); 16]>` with linear scan would eliminate hashing entirely. For the common case of 5-15 fields, all entries fit in 2-3 cache lines.

**3. `FxHashMap` (from `rustc-hash` crate)**
- FxHash is ~3x faster than SipHash for short keys (~5-8ns vs ~20ns).
- Drop-in replacement: `FxHashMap<Vec<u8>, usize>`.
- 300K lookups x 10ns = ~3ms. Saves ~6ms per batch vs current.
- **Recommendation: easiest win.** Minimal code change, no behavior change.

**4. Field index caching across batches**
- Already happening (see analysis above). No work needed.

**5. Hybrid: linear scan + fallback**
- Use a fixed-size array for the first 16 fields (linear scan), fall back to HashMap for overflow.
- Most log schemas have < 16 unique field names, so the HashMap would never be touched in steady state.

### Verdict for #25

- **Impact**: ~6ms savings per batch (FxHashMap) or ~7-8ms (linear scan), roughly 6-8% of a 100ms cycle.
- **Effort**: FxHashMap is a one-line change per builder. Linear scan is ~20 lines.
- **Risk**: Very low. The `resolve_field()` interface does not change. Both approaches are well-understood.
- **Priority**: Low-to-medium. Not a bottleneck, but a genuine quick win. FxHashMap is the best effort/reward ratio. Linear scan is worth benchmarking if we want to squeeze more.

---

## PGO (#26)

### What PGO involves for a Rust binary

Profile-Guided Optimization uses runtime profiling data to guide compiler decisions (branch layout, function inlining, register allocation, cold/hot code splitting).

Steps:
1. **Instrumented build**: `RUSTFLAGS="-Cprofile-generate=/tmp/pgo-data" cargo build --release`
2. **Run representative workload**: exercise the hot path (scanning, building, compression)
3. **Merge profiles**: `llvm-profdata merge -output=/tmp/pgo-data/merged.profdata /tmp/pgo-data/*.profraw`
4. **Optimized build**: `RUSTFLAGS="-Cprofile-use=/tmp/pgo-data/merged.profdata" cargo build --release`

Requires `llvm-tools-preview` rustup component for `llvm-profdata`. The instrumented build is ~2x slower.

### Expected speedup

Published benchmarks for PGO on data-processing workloads:

- **rustc itself**: 10-15% improvement (the canonical Rust PGO success story)
- **ripgrep**: ~10% on search workloads (lots of tight loops over bytes, similar to our scanner)
- **Clang/LLVM**: 15-20% on compilation (branch-heavy code)
- **General rule of thumb**: 5-15% for branch-heavy, loop-heavy code; less for already-vectorized code

For logfwd specifically:
- The scan loop has many branches (JSON token dispatch: `b'"'`, `b'{'`, `b't'`, `b'n'`, number parsing).
- Branch prediction should improve significantly with PGO since log data is regular (most values are strings or small ints).
- The SIMD classification and memchr paths are already well-optimized and may not benefit much.
- **Estimate: 8-12% overall throughput improvement.**

### `target-cpu=native`

Current `[profile.release]` in `Cargo.toml` does not set `target-cpu`.

What `target-cpu=native` enables:
- **x86_64**: AVX2 (256-bit SIMD), BMI1/BMI2 (bit manipulation), FMA, POPCNT, LZCNT. AVX-512 on newer Intel/AMD if available.
- **aarch64 (Apple Silicon)**: NEON is always available (baseline), but `native` enables FEAT_SHA3, FEAT_AES, and potentially SVE on server-class ARM. On Apple M-series, this primarily unlocks crypto acceleration and some scheduling hints.

The `sonic-simd` crate (used for chunk classification) likely already handles SIMD feature detection at runtime. But `target-cpu=native` can help the compiler auto-vectorize other loops (null buffer construction, value copying) and use better instruction scheduling.

**Caveat**: Binaries built with `target-cpu=native` are not portable across CPU generations. Fine for deployment, not for distribution.

### PGO in CI

The nightly benchmark workflow (`bench.yml`) already:
1. Builds release binary
2. Runs competitive benchmarks with 5M lines through multiple scenarios (passthrough, json_parse, filter)

This is a near-perfect PGO training workload. A PGO CI step would look like:

```yaml
- name: PGO - instrumented build
  run: |
    RUSTFLAGS="-Cprofile-generate=/tmp/pgo-data" cargo build --release -p logfwd

- name: PGO - gather profiles
  run: |
    just bench-competitive --lines 2000000 --scenarios json_parse,filter
  env:
    LOGFWD: ./target/release/logfwd

- name: PGO - merge and rebuild
  run: |
    rustup component add llvm-tools-preview
    $(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-profdata \
      merge -output=/tmp/pgo-data/merged.profdata /tmp/pgo-data/*.profraw
    RUSTFLAGS="-Cprofile-use=/tmp/pgo-data/merged.profdata" cargo build --release -p logfwd

- name: PGO - benchmark optimized build
  run: just bench-competitive --lines 5000000 ...
```

This adds ~5 minutes to the workflow (instrumented build + short profile run + rebuild). The reporting step could compare baseline vs PGO numbers in the same issue.

**Concern**: PGO profiles are CPU-architecture-specific. CI runs on `ubuntu-latest` (x86_64), which may differ from deployment targets. If deploying on aarch64, the PGO step would need an ARM runner.

### `target-cpu=native` without affecting CI

Options:

1. **`.cargo/config.toml` with environment override** (recommended):
   ```toml
   # .cargo/config.toml
   [target.'cfg(not(target_env = ""))']
   # No default RUSTFLAGS -- CI and local use different CPUs
   ```
   Then in local builds: `RUSTFLAGS="-Ctarget-cpu=native" cargo build --release`

2. **Justfile target**:
   ```
   build-native:
       RUSTFLAGS="-Ctarget-cpu=native" cargo build --release -p logfwd
   ```

3. **CI explicitly sets `target-cpu=x86-64-v3`** (AVX2 baseline, safe for any modern CI runner):
   ```yaml
   env:
     RUSTFLAGS: "-Ctarget-cpu=x86-64-v3"
   ```
   This is portable across all x86_64 GitHub Actions runners (Haswell or newer).

**Recommendation**: Use `x86-64-v3` in CI for reproducible AVX2 builds. Use `native` locally via Justfile for maximum performance on dev/deploy machines.

### Verdict for #26

- **Impact**: 8-12% throughput improvement (PGO) + 2-5% (target-cpu). Combined: 10-15%.
- **Effort**: Medium. ~30 lines of CI YAML. No code changes.
- **Risk**: Low. PGO is a build-time-only change. The binary is standard Rust, no runtime behavior changes. The main risk is CI time increase (~5 min) and profile staleness if the hot path changes significantly.
- **Priority**: Medium. This is likely the highest ROI optimization available that requires zero code changes. Should be done before any algorithmic changes to the scanner, since PGO improves everything uniformly.

---

## Summary

| Optimization | Est. Impact | Effort | Priority |
|---|---|---|---|
| FxHashMap for field_index | ~6ms/batch (~6%) | Trivial (dep + 2 lines) | Low-medium |
| Linear scan for <16 fields | ~8ms/batch (~8%) | Small (20 lines) | Low-medium, benchmark first |
| PGO in CI | 8-12% throughput | Medium (30 lines YAML) | Medium |
| target-cpu=x86-64-v3 in CI | 2-5% throughput | Trivial (1 env var) | Medium |

Recommended order: target-cpu first (trivial), then PGO (highest bang-for-buck), then FxHashMap (if profiling confirms HashMap is still significant after PGO).
