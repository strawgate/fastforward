# Investigation: io_uring (#31) and Hyperscan (#29)

Research-grade assessment of two performance optimization proposals for logfwd.

---

## io_uring for File Tailing (#31)

### Current Architecture

`FileTailer` in `crates/logfwd-core/src/tail.rs` uses:

- **`notify` crate** (v7) for filesystem events -- kqueue on macOS, inotify on Linux.
  Events are used purely as a **latency hint**, not as the data path.
- **Synchronous `File::read()`** with a 256 KB buffer (`read_buf_size`).
- **Poll interval of 250 ms** as a safety net (inotify misses events on NFS,
  overlayfs, and queue overflow).
- **Directory watching** (not per-file) to catch renames/rotations.
- Files are tracked by `(device, inode, xxhash64_fingerprint)` to handle rotation
  and inode reuse.

The `poll()` method is called in a loop. It drains any pending `notify` events,
then iterates all tracked files and calls `read_new_data()`, which does a blocking
`file.read()` in a loop until it drains all available bytes.

### Would io_uring Help?

**Unlikely to be meaningful for the current workload.** Here is the math:

- Target throughput: 1M lines/sec at ~200 bytes/line = ~200 MB/sec.
- Read buffer: 256 KB. That is ~800 reads/sec across all files.
- Each `read()` syscall costs ~200-500 ns on modern Linux.
- Total syscall overhead: 800 * 500 ns = **0.4 ms/sec** -- negligible.

io_uring's advantage is **batching many syscalls into one kernel transition**.
This matters when you have thousands of small I/O operations per second (e.g.,
database engines doing 100K+ random reads). At 800 reads/sec with 256 KB buffers,
syscall overhead is not the bottleneck.

**Where the CPU actually goes:**
1. JSON/CRI parsing of each log line
2. SQL transform execution (DataFusion query engine)
3. OTLP serialization + zstd compression
4. Memory allocation for line buffers and Arrow arrays

### When Would io_uring Matter?

io_uring could become relevant if:

- **1000+ concurrent log files** with small, frequent writes (e.g., one file per
  microservice container on a dense node). At 1000 files with 4 KB writes, you
  might see 10K+ reads/sec where batching helps.
- **Registered buffers**: io_uring allows pre-registering read buffers to avoid
  per-call `copy_to_user`. This saves ~50 ns/call but again only matters at very
  high operation counts.
- **The tailer becomes async**: Currently `FileTailer::poll()` is synchronous and
  called from a single thread. Converting to io_uring would require rethinking the
  architecture to be async/completion-based.

A more realistic deployment is 10-50 pod log files per node, where io_uring
provides no measurable benefit.

### Rust io_uring Ecosystem

| Crate | Maturity | Tokio Integration | Notes |
|-------|----------|-------------------|-------|
| `io-uring` | Stable, low-level | None (raw SQE/CQE) | Thin wrapper over liburing. Production-ready but requires manual buffer/lifetime management. |
| `tokio-uring` | Experimental | Its own runtime (not standard tokio) | **Cannot share a runtime with regular tokio tasks.** Requires `tokio_uring::start()` instead of `tokio::main`. This is a major architectural constraint. |
| `monoio` | Production at ByteDance | Its own thread-per-core runtime | Not compatible with tokio at all. Would require rewriting the entire async stack. |
| `glommio` | Mature (DataDog) | Its own runtime | Same incompatibility issue as monoio. |

**Key problem**: All io_uring runtimes in Rust use completion-based I/O which is
fundamentally incompatible with tokio's readiness-based model. You cannot mix
`tokio-uring` tasks with regular `tokio::spawn` tasks on the same runtime. logfwd
uses tokio for HTTP output, OTLP, and async coordination -- switching the file
tailing to io_uring would require either:

1. A separate io_uring thread communicating with the tokio runtime via channels, or
2. Rewriting the entire application on a completion-based runtime.

Option 1 is feasible but adds complexity for negligible gain at current scale.

### Platform Portability

io_uring requires Linux 5.1+ (5.6+ recommended for full feature set). logfwd also
runs on macOS for development. This would require:

```rust
#[cfg(target_os = "linux")]
mod io_uring_tailer;

#[cfg(not(target_os = "linux"))]
mod poll_tailer;  // current implementation
```

Two implementations means double the testing surface and maintenance burden.

### Verdict

**Not recommended at this time.** Syscall overhead is not the bottleneck. The
complexity cost (dual implementation, runtime incompatibility, Linux-only) far
outweighs the potential gain. Revisit only if profiling shows >5% CPU time in
syscall overhead at 500+ concurrent files.

**Priority: Low. Effort: High. Expected gain: <1% throughput improvement.**

---

## Hyperscan / Vectorscan for Regex (#29)

### Current Regex Usage

logfwd has two regex-based UDFs in `crates/logfwd-transform/src/udf/`:

1. **`regexp_extract(string, pattern, group_index)`** -- Uses Rust's `regex::Regex`.
   Compiles the pattern once per `invoke_with_args()` call (i.e., once per
   RecordBatch, not once per row). Applies the single pattern to each row
   via `re.captures(val)`.

2. **`grok(string, pattern)`** -- Expands `%{PATTERN:name}` syntax into a regex
   with named capture groups, then compiles via `regex::Regex::new()`. Also
   compiled once per batch invocation. 22 built-in patterns (IP, WORD, NUMBER,
   TIMESTAMP_ISO8601, etc.).

Both UDFs:
- Use the `regex` crate (v1) -- Rust's standard regex engine.
- Compile the pattern **once per batch** (not cached across batches, but DataFusion
  may call `invoke_with_args` once per RecordBatch which is typically 8K-64K rows).
- Apply a **single pattern** per UDF call.

### Hyperscan vs Rust regex: Architecture Mismatch

Hyperscan (Intel, now continued as Vectorscan for ARM) is designed for a
fundamentally different use case:

| Feature | Hyperscan | Rust `regex` |
|---------|-----------|--------------|
| **Primary use case** | Match **many patterns** against one input simultaneously | Match **one pattern** against one input |
| **SIMD acceleration** | Yes (AVX2/AVX-512 for multi-pattern) | Yes (SIMD for literal prefix extraction via aho-corasick and memchr) |
| **Capture groups** | Limited (only in hybrid mode, significant overhead) | Full support, efficient |
| **Streaming mode** | Yes (match across buffer boundaries) | No |
| **Build time** | Slow (compiles to DFA, can take seconds for complex patterns) | Fast (sub-millisecond for typical patterns) |

**Critical mismatch**: logfwd applies **one regex per UDF call**. Hyperscan's
advantage is matching 1000+ patterns simultaneously (e.g., IDS/IPS signature
matching, content filtering). For single-pattern matching with capture groups,
Hyperscan has **no advantage** and may even be slower due to:

- Compilation overhead (Hyperscan compiles patterns to native code via LLVM-like IR)
- Capture group support requires "hybrid" mode which disables many optimizations
- The Rust `regex` crate's DFA/NFA hybrid with SIMD literal optimization is already
  very fast for simple patterns like `status=(\d+)` or `%{IP:addr}`

### Single-Pattern Benchmarks (Industry Data)

For single patterns with capture groups, published benchmarks consistently show:

- Rust `regex` and RE2 are within 10-20% of each other
- Hyperscan in single-pattern mode is comparable to PCRE2-JIT
- For simple literal-anchored patterns (which most log patterns are), Rust `regex`
  is often **faster** than Hyperscan because it uses `memchr`/`aho-corasick` for
  literal prefix scanning before engaging the NFA/DFA

### Where Hyperscan Would Help

Hyperscan would be relevant if logfwd supported:

- **Multi-pattern matching**: e.g., a `classify()` UDF that matches a log line
  against 100+ patterns and returns the first match. This is not currently a
  feature and has not been requested.
- **Streaming regex**: matching patterns that span across chunk boundaries.
  Currently logfwd splits on line boundaries before applying regex, so this
  is not needed.

### Rust Bindings Status

| Crate | Status | Notes |
|-------|--------|-------|
| `hyperscan` | Unmaintained (last update 2022) | Requires Intel's Hyperscan C library. x86-only. |
| `vectorscan` | Active fork | Requires Vectorscan C/C++ library (cmake + boost headers at build time). Supports ARM. |
| `hyperscan-rs` | Minimal | Thin FFI bindings, limited API. |

**Build complexity is significant**: Vectorscan requires cmake, a C++17 compiler,
Boost headers, and PCRE. This adds 30-60 seconds to clean builds and complicates
CI. It would also break the current simple `cargo build` workflow.

### How Often Are Regex UDFs Used?

Both `regexp_extract` and `grok` are **opt-in UDFs** that only execute when the
user's SQL explicitly calls them. Typical logfwd pipelines do:

```sql
SELECT timestamp, severity, message_str, k8s_pod, k8s_namespace
FROM logs
WHERE k8s_namespace = 'production'
```

No regex involved. Regex UDFs are used for unstructured log parsing:

```sql
SELECT regexp_extract(message_str, 'duration=(\d+)ms', 1) AS duration_ms
FROM logs
```

This is a **niche use case** -- most users rely on structured logging (JSON) or
CRI format parsing (which logfwd handles natively without regex).

### Low-Hanging Fruit: Cache Compiled Regex

Before considering Hyperscan, there is a simpler optimization: **cache compiled
regexes across batches**. Currently, `regexp_extract` and `grok` re-compile the
pattern on every `invoke_with_args()` call. Since patterns are typically constant
literals, a `HashMap<String, Regex>` or `OnceCell` cache would eliminate redundant
compilation. This is a 5-line change with zero new dependencies.

### Verdict

**Not recommended.** Hyperscan solves the wrong problem (multi-pattern matching)
for logfwd's use case (single-pattern with capture groups). The Rust `regex` crate
is already well-optimized for this workload. The build complexity, platform
constraints, and maintenance burden are not justified.

**Instead**: Cache compiled regexes across batch invocations. This is free
performance with zero risk.

**Priority: Low. Effort: High. Expected gain: Negligible for single-pattern.**

---

## Summary

| Optimization | Bottleneck Addressed? | Realistic Gain | Complexity | Recommendation |
|-------------|----------------------|----------------|------------|----------------|
| io_uring (#31) | No -- syscall overhead is <0.1% of CPU | <1% throughput | High (dual impl, runtime incompatibility) | **Skip** |
| Hyperscan (#29) | No -- single-pattern perf is already good | Negligible | High (C++ build dep, platform constraints) | **Skip** |
| Regex cache (new) | Yes -- avoids re-compilation per batch | 5-10% for regex-heavy pipelines | Trivial | **Do this instead** |

Both proposals are solutions looking for a problem. Profile first, optimize second.
