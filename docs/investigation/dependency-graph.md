# logfwd Architecture Spec: Dependency Graph & Implementation Order

## Current State Summary

The codebase today has:
- **InputSource trait** (`logfwd-core/src/input.rs`): sync `poll()` returning `Vec<InputEvent>`, only `FileInput` impl
- **FormatParser trait** (`logfwd-core/src/format.rs`): sync `process()` for CRI/JSON/Raw parsing
- **Scanner** (`logfwd-core/src/scanner.rs`): `SimdScanner` and `StreamingSimdScanner`, JSON-to-Arrow
- **SqlTransform** (`logfwd-transform/src/lib.rs`): sync DataFusion execution, creates its own runtime per instance
- **OutputSink trait** (`logfwd-output/src/lib.rs`): sync `send_batch()`, impls for stdout/OTLP/HTTP/jsonlines; ES/Loki/Parquet stubs
- **Pipeline** (`logfwd/src/pipeline.rs`): single-threaded poll loop, `AtomicBool` shutdown, no checkpointing

## Component Definitions

| # | Component | What Changes |
|---|-----------|-------------|
| 1 | **Source trait** | Merge `InputSource` + `FormatParser` into unified async `Source` producing Arrow batches |
| 2 | **Scanner contract** | No code change; formalize boundary tests for SimdScanner/StreamingSimdScanner |
| 3 | **DiskQueue** | New module: Arrow IPC write/read for persistence between Source and Sink |
| 4 | **Transform async** | Refactor `SqlTransform` to accept a shared Tokio runtime instead of creating its own |
| 5 | **Sink trait** | Replace sync `OutputSink` with async trait + retry/backpressure |
| 6 | **Pipeline orchestrator** | Async task graph with tokio channels replacing the poll loop |
| 7 | **CheckpointStore** | New module: JSON file persistence for offsets (file position, kafka offset, etc.) |

## Pairwise Dependency Analysis

### Core Components (1-7)

| A | B | Relationship | Rationale |
|---|---|-------------|-----------|
| 1 (Source) | 2 (Scanner contract) | **Independent** | Scanner contract formalizes existing behavior; Source consumes scanner output but doesn't change the scanner API |
| 1 (Source) | 3 (DiskQueue) | **A blocks B** | DiskQueue sits between Source and Sink; needs to know the batch format Source produces |
| 1 (Source) | 4 (Transform async) | **Independent** | Source produces RecordBatch; Transform consumes RecordBatch. Interface is Arrow, decoupled today |
| 1 (Source) | 5 (Sink) | **Independent** | No direct dependency; connected through Pipeline |
| 1 (Source) | 6 (Pipeline) | **A blocks B** | Pipeline orchestrator wires Source as an async task; needs the Source trait defined |
| 1 (Source) | 7 (Checkpoint) | **A blocks B** | CheckpointStore records Source offsets; needs to know what offset type Source exposes |
| 2 (Scanner contract) | 3 (DiskQueue) | **Independent** | DiskQueue is Arrow IPC, scanner is JSON-to-Arrow; no overlap |
| 2 (Scanner contract) | 4 (Transform) | **Independent** | Scanner feeds Transform, but contract tests don't change the interface |
| 2 (Scanner contract) | 5 (Sink) | **Independent** | No relationship |
| 2 (Scanner contract) | 6 (Pipeline) | **Independent** | Contract tests validate existing scanner; Pipeline consumes scanner output unchanged |
| 2 (Scanner contract) | 7 (Checkpoint) | **Independent** | No relationship |
| 3 (DiskQueue) | 4 (Transform) | **Independent** | DiskQueue persists pre-transform or post-transform batches; neither depends on the other's internals |
| 3 (DiskQueue) | 5 (Sink) | **Independent** | DiskQueue feeds Sink but through Pipeline; Arrow IPC format is self-describing |
| 3 (DiskQueue) | 6 (Pipeline) | **A blocks B** | Pipeline must wire DiskQueue between stages; DiskQueue module must exist first |
| 3 (DiskQueue) | 7 (Checkpoint) | **Should be done together** | DiskQueue needs checkpoint coordination: "what has been durably written" and "what has been consumed". CheckpointStore tracks both file offsets and queue replay positions |
| 4 (Transform) | 5 (Sink) | **Independent** | Transform produces RecordBatch, Sink consumes it; no coupling |
| 4 (Transform) | 6 (Pipeline) | **A blocks B** | Pipeline must spawn Transform as an async task; needs the async-ready interface |
| 4 (Transform) | 7 (Checkpoint) | **Independent** | Transform is stateless per-batch; nothing to checkpoint |
| 5 (Sink) | 6 (Pipeline) | **A blocks B** | Pipeline wires Sink as an async task with backpressure |
| 5 (Sink) | 7 (Checkpoint) | **A blocks B** | CheckpointStore advances offset only after Sink confirms delivery (at-least-once) |
| 6 (Pipeline) | 7 (Checkpoint) | **B blocks A** (partial) | Pipeline needs CheckpointStore to implement at-least-once. But Pipeline can ship an initial version without checkpointing (at-most-once), then add it |

### Related Work vs. Core Components

| Related Work | Depends On | Rationale |
|-------------|-----------|-----------|
| SQL rewriter | 4 (Transform async) | Rewrites SQL before Transform; can be built independently but should land after Transform async so it shares the runtime |
| Signal handling | 6 (Pipeline) | Replaces `AtomicBool` with `CancellationToken`; trivial standalone change but most useful when Pipeline is async |
| Network inputs (TCP/UDP/OTLP) | 1 (Source trait) | New `Source` implementations; blocked until Source trait is defined |
| New sinks (ES/Loki/file/Parquet) | 5 (Sink trait) | New `Sink` implementations; blocked until async Sink trait is defined |
| StreamingSimdScanner default | 2 (Scanner contract) | Should land after boundary tests confirm correctness |
| Scanner boundary tests (#76-79) | **None** | Can start immediately; tests existing code |

## Dependency DAG

```
                    Scanner contract (2) -----> StreamingSimdScanner default
                          |                            |
                    (independent)                      |
                          |                            v
  Signal handling --------|------ Scanner boundary tests (#76-79)
        |
        v
  Source trait (1) -----------> Network inputs (TCP/UDP/OTLP)
        |       \
        |        \--------> CheckpointStore (7) <------- Sink trait (5) --> New sinks
        |                       |       \                    |
        v                       |        \                   |
  DiskQueue (3) ------+--------+         |                   |
        |             |                  |                   |
        v             v                  v                   v
  Transform async (4) -----> Pipeline orchestrator (6) <-----+
        |                        |
        v                        v
  SQL rewriter             Signal handling (final form)
```

## Proposed Implementation Order

### Phase 0: Risk Reduction (Week 1)
**Goal:** Validate assumptions, no production code changes needed.

| Track | Work Item | People | Risk Addressed |
|-------|-----------|--------|----------------|
| A | Scanner boundary tests (#76-79) | 1 | Confirms SimdScanner/StreamingSimdScanner correctness before we build on them |
| B | DiskQueue spike: Arrow IPC write/read roundtrip benchmark | 1 | Validates that Arrow IPC + mmap can sustain 1M+ lines/sec persistence |

**Deliverable:** All existing tests pass + new scanner boundary tests. DiskQueue spike has throughput numbers.

### Phase 1: Trait Definitions (Week 2)
**Goal:** Define the new trait contracts. Each PR is small, mergeable, and the existing pipeline keeps working.

| Track | Work Item | Depends On | People |
|-------|-----------|-----------|--------|
| A | Source trait: define trait + adapt `FileInput` as first impl | Phase 0 complete | 1 |
| B | Sink trait: define async trait + adapt existing `OutputSink` impls | None | 1 |
| C | StreamingSimdScanner as default (swap in Pipeline) | Scanner boundary tests pass | 1 |

**Parallelism: 3 people can work simultaneously.**

**Deliverable:** New traits exist. Old impls conform. Pipeline still works with `FileInput` -> `Scanner` -> `SqlTransform` -> `StdoutSink`. No async yet in the pipeline itself.

### Phase 2: Async Internals (Weeks 3-4)
**Goal:** Make components async-ready. Pipeline still single-threaded but internals are async-compatible.

| Track | Work Item | Depends On | People |
|-------|-----------|-----------|--------|
| A | Transform async: shared Tokio runtime, `async execute()` | Source trait (for RecordBatch contract stability) | 1 |
| B | DiskQueue implementation: Arrow IPC persistence module | Source trait (for batch format), spike results | 1 |
| C | CheckpointStore: JSON file persistence | Source trait (for offset types), Sink trait (for ack protocol) | 1 |
| D | Signal handling: `CancellationToken` in existing pipeline | None | 1 |

**Parallelism: 4 people can work simultaneously.**

**Deliverable:** Each component works standalone with unit tests. Pipeline still uses the old poll loop but components are internally async-ready.

### Phase 3: Pipeline Integration (Weeks 5-6)
**Goal:** Wire everything together into the async Pipeline orchestrator.

| Track | Work Item | Depends On | People |
|-------|-----------|-----------|--------|
| A | Pipeline orchestrator: async task graph with tokio channels | Source, Sink, Transform async, DiskQueue, CheckpointStore, Signal handling | 2 |
| B | SQL rewriter (user-friendly SQL -> typed-column SQL) | Transform async | 1 |

**Parallelism: 2 tracks. Pipeline is the critical path and benefits from pair work.**

**Deliverable:** Full async pipeline: Source -> Scanner -> DiskQueue -> Transform -> Sink, with checkpointing and graceful shutdown.

### Phase 4: New Implementations (Weeks 7+)
**Goal:** Expand Source and Sink coverage. Each is an independent PR.

| Track | Work Item | Depends On | People |
|-------|-----------|-----------|--------|
| A | TCP Source | Source trait | 1 |
| B | UDP Source | Source trait | 1 |
| C | OTLP Source | Source trait | 1 |
| D | Elasticsearch Sink | Sink trait | 1 |
| E | Loki Sink | Sink trait | 1 |
| F | File/Parquet Sink | Sink trait | 1 |

**Parallelism: Up to 6 people. All independent.**

## Critical Path

```
Scanner boundary tests -> StreamingSimdScanner default
Source trait -> DiskQueue -> Pipeline orchestrator
Source trait -> CheckpointStore -> Pipeline orchestrator
Sink trait -> Pipeline orchestrator
Transform async -> Pipeline orchestrator
```

**Longest path:** Source trait (1 week) -> DiskQueue + CheckpointStore (2 weeks) -> Pipeline orchestrator (2 weeks) = **5 weeks minimum**.

## Risk Mitigation Notes

1. **DiskQueue is the riskiest new component.** It involves Arrow IPC serialization, mmap reads, file rotation, and crash recovery. The Phase 0 spike validates feasibility before committing to the design.

2. **Async migration is incremental, not big-bang.** Each component gets an async interface behind a trait. The pipeline poll loop can call `.block_on()` for each async component during the transition. Only Phase 3 flips the top-level loop to async.

3. **CheckpointStore + DiskQueue coupling.** These share the concept of "durable position." Building them in the same phase (but separate PRs) lets the interfaces co-evolve without blocking each other.

4. **Source trait must be right.** It is on the critical path for 4 downstream items. Spend extra review time on the trait signature. Key decisions: does Source produce raw bytes or RecordBatch? (Recommendation: RecordBatch, since Scanner is internal to Source.) Does Source own the FormatParser? (Recommendation: yes, Source = read + parse + scan.)

5. **Backward compatibility.** Every phase produces a working pipeline. Phase 1 adapts existing impls to new traits. Phase 2 makes internals async but the pipeline loop stays sync. Phase 3 is the only phase that changes the top-level execution model.
