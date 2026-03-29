# Sync-to-Async Migration Investigation

Research-only analysis of migrating the logfwd pipeline from synchronous threading to async/await.

---

## 1. Current Threading Model

**Pipeline spawning** (`main.rs:303-313`): Each pipeline runs on a dedicated OS thread via `std::thread::spawn`. The last pipeline runs on the main thread. Shutdown is coordinated via a shared `Arc<AtomicBool>`.

**Pipeline loop** (`pipeline.rs:113-224`): Entirely synchronous. A `loop` polls inputs every 10ms (`poll_interval`), accumulates data into `json_buf`, and flushes batches when size (4MB) or timeout (100ms) thresholds are hit. When idle, it calls `std::thread::sleep(poll_interval)`.

**Where tokio appears today**:
- `logfwd-transform/src/lib.rs:566` -- `SqlTransform::execute()` creates a **new** `tokio::runtime::Builder::new_current_thread()` runtime on **every call** to `execute()`, because DataFusion's API is async. This runtime is dropped after each `block_on()`.
- `crates/logfwd/src/main.rs:458` -- `build_meter_provider()` creates a `tokio::runtime::Builder::new_multi_thread()` with 1 worker thread for the OTel metrics periodic exporter. The runtime is intentionally leaked via `std::mem::forget(rt)` so the exporter thread stays alive.
- UDF tests in `logfwd-transform` create per-test current-thread runtimes.

**Key observation**: The hot path (scan + transform + output) is fully sync except for the DataFusion `block_on` inside `SqlTransform::execute()`. No async anywhere in Source, Sink, or the pipeline loop itself.

---

## 2. async_trait Overhead

`#[async_trait]` (the `async-trait` crate) desugars `async fn` in traits to `fn -> Pin<Box<dyn Future>>`. This means:

- **One heap allocation per call** (the boxed future).
- Typical cost: 20-50ns for the alloc + dealloc on modern allocators (jemalloc/mimalloc).

**For the proposed call frequencies**:
- `Source::poll()` at ~10ms intervals = ~100 calls/sec. Overhead: ~5 microseconds/sec total. **Negligible.**
- `Sink::send_batch()` at ~100ms intervals = ~10 calls/sec. Overhead: <1 microsecond/sec total. **Negligible.**
- `FormatParser::process()` -- called per-event in the inner loop, potentially 100K+ calls/sec. **Do NOT make this async.** Keep it sync.

**Recommendation**: `async_trait` overhead is acceptable for Source and Sink trait methods. The Scanner and FormatParser must remain sync -- they are the hot path. Note: Rust 1.75+ has native async fn in traits (AFIT), which avoids the heap allocation entirely. If the MSRV permits it, prefer native AFIT over `#[async_trait]`.

**Native AFIT consideration**: With Rust 1.75+ (`async fn` in traits), the return type is an opaque `impl Future` on the stack -- zero heap allocation. The tradeoff is that the trait is no longer object-safe (`dyn Trait` won't work). For `dyn OutputSink` dispatch via `Box<dyn OutputSink>`, you'd still need either `#[async_trait]` or a manual enum dispatch. Since the codebase already uses `Box<dyn OutputSink>` and `Box<dyn InputSource>`, `#[async_trait]` or an enum-based approach is required.

---

## 3. Incremental Migration Strategy

### Phase 1: Async traits with `block_on()` bridge

Define new async versions of the traits:
```rust
#[async_trait]
pub trait AsyncOutputSink: Send {
    async fn send_batch(&mut self, batch: &RecordBatch, metadata: &BatchMetadata) -> io::Result<()>;
    async fn flush(&mut self) -> io::Result<()>;
    fn name(&self) -> &str;
}
```

Keep the existing sync `Pipeline::run()` loop. Call async methods via `block_on()`.

### Phase 2: Make the pipeline loop async

Replace the sync `loop` with a `tokio::select!`-based event loop.

### Gotchas with `block_on()` inside existing tokio runtimes

**This is the critical risk.** Calling `tokio::runtime::Runtime::block_on()` from within an existing tokio runtime panics:

```
Cannot start a runtime from within a runtime.
```

Current state: `SqlTransform::execute()` creates a **new** current-thread runtime each call. This works because the pipeline thread is a plain OS thread, not a tokio worker thread. But:

- **Phase 1 is safe** as long as the pipeline loop remains on a plain OS thread. `block_on()` on a fresh runtime works fine from a non-tokio thread.
- **Phase 2 breaks** if you naively wrap the pipeline in `tokio::spawn` while SqlTransform still creates its own runtime. You'd get nested runtimes. Fix: refactor SqlTransform to accept an external runtime handle, or use `tokio::task::block_in_place()` (only available on multi-thread runtime).
- The `build_meter_provider` tokio runtime is leaked but lives on its own threads, so it won't conflict.

**Mitigation path for Phase 2**: Before making the pipeline loop async, refactor `SqlTransform::execute()` to be `async fn execute()` that calls DataFusion natively without creating its own runtime. This is straightforward since DataFusion is already async -- just remove the `Runtime::new()` + `block_on()` wrapper and let the caller's runtime drive it.

---

## 4. tokio::sync::mpsc for Backpressure

The spec proposes bounded mpsc channels between pipeline stages.

**Overhead characteristics** (based on tokio's implementation):
- `tokio::sync::mpsc` uses a linked-list of blocks (32 entries each) with atomic operations.
- **Empty channel send**: ~40-60ns (uncontended, single producer/consumer).
- **Empty channel recv**: ~40-60ns when data is available.
- **At capacity (backpressure)**: sender suspends (yields to the async runtime). The cost is a future park + wake cycle, ~100-200ns when the receiver drains.
- **Throughput**: Published benchmarks show 10-50M messages/sec for small messages on a single producer/consumer pair.

**For logfwd's workload**: The pipeline does not pass individual lines through channels. It passes **batches** (Arrow RecordBatch objects, typically 4MB / ~10K-50K rows). At ~10 batches/sec, channel overhead is completely irrelevant. Even at 100 batches/sec the overhead would be microseconds.

**Recommendation**: `tokio::sync::mpsc` with a small bound (2-4) is fine. The channel is not on the hot path -- the hot path is scanning/transforming within a single task. Channels only bridge between stages.

**Alternative considered**: `crossbeam-channel` is already in `logfwd-core` dependencies (though not used in source code currently). It's slightly faster for sync-to-sync but doesn't integrate with `select!`. For an async pipeline, `tokio::sync::mpsc` is the right choice.

---

## 5. CancellationToken

**Not currently in the dependency tree.** `tokio-util` is not in `Cargo.lock`. Only `tokio`, `tokio-macros`, and `tokio-stream` are present.

**What needs to be added**: `tokio-util` with the `sync` feature (for `CancellationToken`).

**API**:
```rust
use tokio_util::sync::CancellationToken;

let token = CancellationToken::new();
let child = token.child_token();  // hierarchical cancellation

// In select! loop:
tokio::select! {
    data = channel.recv() => { /* process */ }
    _ = token.cancelled() => { break; }
}

// To trigger shutdown:
token.cancel();
```

**Key properties**:
- `CancellationToken` is `Clone + Send + Sync` -- no `Arc` wrapper needed (it uses `Arc` internally).
- `child_token()` enables hierarchical shutdown: cancelling a parent cancels all children.
- `.cancelled()` returns a future that completes when the token is cancelled. Zero-cost polling when not cancelled (it's a `Notified` future).
- Integrates naturally with `select!` -- cleaner than checking `AtomicBool` in a loop.

**Migration from AtomicBool**: Straightforward. Replace `shutdown.load(Ordering::Relaxed)` checks with `select!` branches on `token.cancelled()`. The AtomicBool pattern requires polling; CancellationToken is event-driven.

---

## 6. spawn_blocking for Sync Sinks

**Current sync sinks**:
- `StdoutSink`: calls `io::stdout().lock()` then writes. This acquires a mutex on every batch.
- `OtlpSink`: calls `ureq::post().send()` -- a full synchronous HTTP request with TLS, compression, network I/O.
- `JsonLinesSink`: same pattern -- `ureq::post().send()`.

**spawn_blocking overhead**: ~1-3 microseconds to schedule a closure onto the blocking thread pool, plus a thread context switch. For calls happening every 100ms, this is negligible.

**When spawn_blocking is needed vs. not**:

| Sink | Blocking? | spawn_blocking needed? | Rationale |
|------|-----------|----------------------|-----------|
| StdoutSink | Briefly (mutex) | **No** | stdout writes to a pipe/terminal are fast (<1us for typical batch sizes). Holding a tokio worker for <1ms is fine. Only if stdout is redirected to a slow consumer. |
| OtlpSink | **Yes** (network I/O) | **Yes** | `ureq` is a blocking HTTP client. A single request can take 10-500ms. This would block the tokio worker thread. |
| JsonLinesSink | **Yes** (network I/O) | **Yes** | Same as OtlpSink. |
| FileSink (future) | Briefly | Probably no | File writes to local disk are typically fast. Could become an issue with NFS mounts. |

**Simpler alternative for HTTP sinks**: Instead of `spawn_blocking` + `ureq`, switch to an async HTTP client (`reqwest` or `hyper`). This is more idiomatic and avoids the thread pool overhead entirely. `reqwest` is already an indirect dependency via `opentelemetry-otlp` (the workspace uses the `reqwest-client` feature). This would be a better long-term approach but is a larger change.

**Recommendation**: For Phase 1, keep `ureq` + `spawn_blocking`. For Phase 2, consider switching network sinks to `reqwest` (already in the dep tree transitively). StdoutSink can call `write()` directly in async context.

---

## 7. Risk Assessment

### High Risk

**Nested runtime panic**: The most likely failure mode. If Phase 2 runs the pipeline loop on a tokio runtime while `SqlTransform::execute()` still creates its own runtime, it will panic. **Mitigation**: Refactor SqlTransform before Phase 2. Make `execute()` async and remove the internal runtime. This is prerequisite work.

**Performance regression from async overhead on hot path**: The Scanner processes 1M+ lines/sec using SIMD. If the scanner or format parser are accidentally made async, the overhead of future state machines, heap allocations, and lost auto-vectorization could cause a significant regression. **Mitigation**: Keep Scanner and FormatParser as sync functions. Only the pipeline orchestration (poll, flush, send) becomes async.

### Medium Risk

**Backpressure stalls causing latency spikes**: Bounded channels can cause the producer to suspend when the consumer is slow (e.g., HTTP sink is doing a slow request). If the channel is too small, the scanner stalls and tail reading stops, causing log data to back up. **Mitigation**: Size channels appropriately (2-4 batches), add metrics on channel fullness, and ensure sinks use timeouts.

**OTel metrics runtime conflict**: The leaked tokio runtime in `build_meter_provider` runs on separate OS threads. When the pipeline moves to its own tokio runtime, there will be two runtimes. This should be fine (they're independent), but the leaked runtime is a code smell. **Mitigation**: In Phase 2, share a single runtime. Pass the handle to the meter provider builder.

**Testing complexity**: Async tests require `#[tokio::test]`. Integration tests that currently use `std::thread::spawn` + `AtomicBool` will need restructuring. **Mitigation**: Migrate tests incrementally alongside the code.

### Low Risk

**async_trait overhead**: As analyzed in section 2, the overhead is negligible for the call frequencies involved. Not a real risk.

**tokio::sync::mpsc overhead**: As analyzed in section 4, channel throughput far exceeds the batch rate. Not a real risk.

**CancellationToken**: Drop-in replacement for AtomicBool with a cleaner API. Low risk.

### Recommended Migration Order

1. **Prep**: Refactor `SqlTransform::execute()` to be `async` internally (remove the per-call runtime creation). Have it accept a runtime handle or be called from an async context.
2. **Phase 1**: Define `#[async_trait]` versions of `OutputSink` and `InputSource`. Implement them. Keep the sync pipeline loop; use `block_on()` to call async methods from the OS thread.
3. **Phase 1.5**: Add `tokio-util` dependency. Replace `AtomicBool` shutdown with `CancellationToken`.
4. **Phase 2**: Convert `Pipeline::run()` to `async fn run()`. Use `tokio::select!` for the event loop. Add bounded mpsc channels between stages if fan-out or stage decoupling is needed.
5. **Phase 2.5**: Replace `ureq` with `reqwest` in network sinks (optional, can stay with `spawn_blocking` + `ureq`).

### What to Benchmark Before/After

- End-to-end throughput: lines/sec with `--generate-json 1000000` piped through a transform to stdout/blackhole.
- Latency: p50/p99 batch flush time (scan + transform + output).
- Memory: heap profile with dhat to detect async-related allocations.
- CPU: flamegraph comparison to verify the scanner hot path is not affected.
