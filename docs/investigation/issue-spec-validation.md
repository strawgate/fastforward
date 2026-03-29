# Issue-to-Architecture Spec Validation

Validates every open GitHub issue against the Pipeline Architecture Specification
(`docs/PIPELINE_ARCHITECTURE.md`). 41 open issues checked.

**Spec reference:** PIPELINE_ARCHITECTURE.md key decisions: Source trait (async poll,
checkpoint, ack, SourceId), Scanner stays concrete (SimdScanner, no trait), DiskQueue
after scanner before transform (Arrow IPC on disk), Transform becomes async (shares
tokio runtime), Sink becomes async with SendResult::Ok/RetryAfter/Rejected, Pipeline
uses bounded tokio::sync::mpsc channels for backpressure, CancellationToken for
shutdown, Per-source identity (SourceId) preserved end-to-end, CheckpointStore for
crash recovery, PipelineRuntimeConfig for tunables.

---

## Legend

- **Fully addressed** -- the spec's design directly solves the issue
- **Partially addressed** -- spec provides the framework but issue needs its own implementation work
- **Not addressed** -- independent of the architecture spec
- **Conflict** -- issue conflicts with a spec decision

---

## Summary Table

| Issue | Title | Status | Notes |
|-------|-------|--------|-------|
| #23 | SIMD JSON parser | Not addressed | Scanner internals, spec intentionally leaves scanner as concrete |
| #24 | Zstd dictionary training | Not addressed | Compression optimization, orthogonal to pipeline architecture |
| #25 | String interning / perfect hashing | Not addressed | Scanner internals optimization |
| #26 | PGO and target-cpu=native | Not addressed | Build system optimization |
| #27 | Columnar OTLP encoding | Partially addressed | Spec defines async Sink trait; encoding strategy is within sink impl |
| #28 | Adaptive batch sizing | Fully addressed | PipelineRuntimeConfig exposes batch_target_bytes + batch_timeout |
| #29 | Hyperscan/Vectorscan regex UDFs | Not addressed | UDF internals, orthogonal to pipeline |
| #30 | Arrow Flight / Arrow IPC output sink | Partially addressed | Spec defines Sink trait; Arrow Flight is a new sink impl |
| #31 | io_uring file tailing | Partially addressed | Spec defines Source trait with async poll(); io_uring is an impl detail |
| #32 | Containerd shim log plugin | Partially addressed | Spec's Source trait supports non-file inputs via SourceOutput::Raw |
| #34 | Typed input/output config | Not addressed | Config parsing, orthogonal to runtime architecture |
| #35 | Per-input batching with source context | Fully addressed | SourceId preserved end-to-end, BatchMetadata carries source_id |
| #36 | Enrichment via DataFusion JOINs/UDFs | Partially addressed | Spec's async Transform reuses DataFusion context; enrichment tables are additive |
| #37 | Background enrichment providers | Partially addressed | Spec's Transform caches SessionContext; enrichment tables register into it |
| #39 | geo_lookup() UDF | Not addressed | UDF implementation, orthogonal to pipeline |
| #40 | Signal handler + graceful shutdown | Fully addressed | CancellationToken replaces AtomicBool; ordered drain specified |
| #41 | Output retry with exponential backoff | Fully addressed | Sink trait returns SendResult::RetryAfter; spec defines backoff params |
| #42 | HTTP timeout configuration | Partially addressed | PipelineRuntimeConfig has retry_base/retry_max; HTTP timeouts are sink-internal |
| #43 | K8s DaemonSet manifest | Not addressed | Deployment config, orthogonal to runtime architecture |
| #44 | File offset checkpointing | Fully addressed | CheckpointStore trait + SourceCheckpoint with position bytes |
| #45 | Backpressure / memory limits | Fully addressed | Bounded tokio::sync::mpsc channels; DiskQueue with queue_max_bytes limit |
| #46 | Structured logging with tracing | Not addressed | Observability infrastructure, orthogonal to pipeline architecture |
| #47 | Pipeline tuning configurable in YAML | Fully addressed | PipelineRuntimeConfig exposes all tunables (channel_capacity, batch_target_bytes, batch_timeout, checkpoint_interval, queue_max_bytes, retry_base, retry_max) |
| #48 | Replace .unwrap()/.expect() panics | Not addressed | Code quality, orthogonal to architecture |
| #49 | CI: Docker, security audit, platform matrix | Not addressed | CI/CD infrastructure |
| #50 | Release automation | Not addressed | CI/CD infrastructure |
| #51 | Integration test suite | Partially addressed | Spec defines component contracts that enable integration testing; tests themselves are separate work |
| #52 | Documentation | Not addressed | Documentation effort, orthogonal |
| #53 | Implement output sinks (ES, Loki, file, Parquet) | Partially addressed | Spec defines Sink trait + SendResult contract; each sink is its own impl |
| #54 | Network input sources (TCP, UDP, syslog) | Partially addressed | Spec defines Source trait with async poll + SourceOutput::Raw; each source is its own impl |
| #67 | Workspace clippy pedantic lints | Not addressed | Code quality tooling |
| #69 | Wire StreamingSimdScanner as default | Not addressed | Scanner selection, spec keeps scanner concrete without prescribing which impl |
| #70 | Adaptive dictionary encoding | Not addressed | StorageBuilder internals, orthogonal to pipeline |
| #71 | Compressed Arrow IPC output | Partially addressed | DiskQueue spec uses Arrow IPC on disk; this issue provides the building block |
| #72 | Disk queue for multi-output replay | Fully addressed | DiskQueue component spec directly addresses this (Arrow IPC segments, manifest, replay, ack) |
| #74 | DiagnosticsServer bind failure panics | Not addressed | Bug fix, orthogonal to pipeline architecture |
| #75 | Pipeline shutdown flag never set | Fully addressed | CancellationToken replaces AtomicBool; spec defines ordered shutdown drain |
| #76 | Formalize scanner contract | Partially addressed | Spec documents scanner data contract (input: complete NDJSON, output: type-suffixed columns, null-padded); full formalization is broader |
| #77 | Fuzz the SIMD scanner | Not addressed | Testing effort, orthogonal |
| #78 | Scanner-DataFusion boundary testing | Partially addressed | Spec defines scanner output contract + transform input contract; testing is separate |
| #79 | Scanner-output sink serialization testing | Partially addressed | Spec defines column naming convention + sink stripping rules; testing is separate |

---

## Detailed Analysis

### Fully Addressed (8 issues)

**#28 - Adaptive batch sizing**
PipelineRuntimeConfig exposes `batch_target_bytes` (default 4 MiB) and
`batch_timeout` (default 100ms) as runtime tunables. The issue asks for adaptive
sizing based on throughput feedback -- the config provides the knobs, and the bounded
channel backpressure provides the feedback signal. The adaptive algorithm itself would
build on top of these primitives.

**#35 - Per-input batching with source context**
This is one of the spec's core decisions. SourceId is preserved end-to-end through
BatchMetadata. Every batch carries `source_id`, enabling per-source enrichment,
routing, and checkpointing. The issue's exact problem (losing source identity after
scan) is eliminated.

**#40 - Signal handler + graceful shutdown**
Spec replaces the broken `AtomicBool` with `CancellationToken`. Shutdown order is
explicitly defined: sources stop -> scanner finishes -> queues flush -> transform
completes -> sinks flush -> checkpoints persist. This directly solves the data loss
on SIGTERM described in the issue.

**#41 - Output retry with exponential backoff**
Sink trait returns `SendResult::RetryAfter(Duration)`. Spec defines retry semantics:
exponential backoff (base=1s, max=60s, jitter=500ms). `Rejected` drops the batch
with a WARN log. `io::Error` is treated as transient and retried. Pre-output
DiskQueue provides the replay buffer for retry.

**#44 - File offset checkpointing**
CheckpointStore trait with `update()` (in-memory), `flush()` (persist to disk),
`load()` / `load_all()`. SourceCheckpoint carries `source_id`, opaque `position`
bytes (file: device+inode+offset), and `timestamp_ns`. Flushed every 5s + on shutdown
via atomic rename.

**#45 - Backpressure / memory limits**
Bounded `tokio::sync::mpsc` channels between all stages. Sink slow -> pre-output
queue fills -> transform blocks -> pre-transform queue fills -> scanner stops ->
source stops polling. DiskQueue has `queue_max_bytes` (default 1 GiB) as a hard
limit. `push()` blocks when full.

**#47 - Pipeline tuning configurable in YAML**
PipelineRuntimeConfig exposes: `channel_capacity` (16), `batch_target_bytes` (4 MiB),
`batch_timeout` (100ms), `checkpoint_interval` (5s), `queue_max_bytes` (1 GiB),
`retry_base` (1s), `retry_max` (60s). All values from the issue are covered.

**#72 - Disk queue for multi-output replay**
DiskQueue component is specified with: Arrow IPC files per segment, manifest.json for
read/ack tracking, `push()`/`peek()`/`ack()`/`replay()` API, size limit with
backpressure, and crash recovery via replay of unacked entries. Two named segments
(pre-transform and pre-output) enable the multi-output replay the issue describes.

**#75 - Pipeline shutdown flag never set**
CancellationToken propagates cancellation to all tasks. The spec's ordered drain
ensures every task observes shutdown. This directly fixes the issue where sibling
pipeline threads never see the shutdown signal.

### Partially Addressed (14 issues)

**#27 - Columnar OTLP encoding**
The Sink trait defines the interface (`send_batch` receives RecordBatch); columnar vs
row-at-a-time encoding is an implementation detail within the OTLP sink. The spec
enables but does not prescribe this optimization.

**#30 - Arrow Flight / Arrow IPC output sink**
A new sink implementation under the Sink trait. The spec's universal Arrow
RecordBatch interchange format makes this natural -- no format conversion needed.
Implementation is additive.

**#31 - io_uring file tailing**
Source trait's async `poll()` is compatible with io_uring (tokio-uring or similar).
The trait doesn't constrain the I/O backend. Implementation is within the file source.

**#32 - Containerd shim log plugin**
Source trait with `SourceOutput::Raw` supports any bytes-producing input, including a
shim log plugin that receives container stdout/stderr via pipe. Implementation is a
new Source impl.

**#36 - Enrichment via DataFusion JOINs/UDFs**
The async Transform caches DataFusion `SessionContext`. Enrichment tables can be
registered as MemTables in that context. The spec provides the runtime; enrichment
table management is additive.

**#37 - Background enrichment providers**
Same as #36. The cached SessionContext can host dynamically-refreshed tables. A
background refresh mechanism is new work that plugs into the existing Transform
infrastructure.

**#42 - HTTP timeout configuration**
PipelineRuntimeConfig has retry parameters. HTTP-specific timeouts (connect, read,
write) are internal to sink implementations. The spec doesn't define per-sink config,
but the framework supports it.

**#51 - Integration test suite**
The spec formalizes component contracts (input/output types, error semantics,
backpressure behavior) which define what integration tests should verify. Writing
the tests is separate work.

**#53 - Implement output sinks (ES, Loki, file, Parquet)**
Sink trait + SendResult contract provides the interface. Each new sink is an
implementation of `async fn send_batch()`. The spec enables but each sink requires
its own protocol-specific work.

**#54 - Network input sources (TCP, UDP, syslog)**
Source trait with async `poll()` and `SourceOutput::Raw` handles network inputs
naturally. Each listener is a Source impl. The issue's concern about sync poll() is
resolved -- the spec makes Source async.

**#71 - Compressed Arrow IPC output from StorageBuilder**
DiskQueue uses Arrow IPC on disk. This issue provides the `finish_batch_compressed()`
building block. Partially addressed because the spec depends on this capability but
doesn't specify the StorageBuilder API.

**#76 - Formalize scanner contract**
The spec documents the scanner's data contract (input: complete NDJSON lines, output:
type-suffixed columns, null-padded, _raw exception). The issue asks for broader
formalization including UTF-8 requirements, nested JSON handling, and partial line
behavior, which go beyond what the spec covers.

**#78 - Scanner-DataFusion boundary testing**
The spec defines both scanner output and transform input contracts, which implicitly
define the boundary. Testing that boundary is separate work.

**#79 - Scanner-output sink serialization testing**
The spec's column naming convention and sink suffix-stripping rules define the
contract. Testing edge cases (escaped strings, nested JSON, Utf8View) is separate.

### Not Addressed (19 issues)

These are independent of the pipeline architecture:

- **#23** - SIMD JSON parser (scanner internals)
- **#24** - Zstd dictionary training (compression optimization)
- **#25** - String interning / perfect hashing (scanner internals)
- **#26** - PGO and target-cpu=native (build optimization)
- **#29** - Hyperscan/Vectorscan regex UDFs (UDF internals)
- **#34** - Typed input/output config (config parsing)
- **#39** - geo_lookup() UDF (UDF implementation)
- **#43** - K8s DaemonSet manifest (deployment)
- **#46** - Structured logging with tracing (observability)
- **#48** - Replace .unwrap()/.expect() panics (code quality)
- **#49** - CI: Docker, security audit, platform matrix (CI/CD)
- **#50** - Release automation (CI/CD)
- **#52** - Documentation (docs)
- **#67** - Workspace clippy pedantic lints (tooling)
- **#69** - Wire StreamingSimdScanner as default (scanner selection)
- **#70** - Adaptive dictionary encoding (StorageBuilder internals)
- **#74** - DiagnosticsServer bind failure panics (bug fix)
- **#77** - Fuzz the SIMD scanner (testing)

### Conflicts (0 issues)

No open issues conflict with spec decisions.

---

## Dependency Map

Issues have the following dependency relationships relevant to the spec:

```
#71 (Arrow IPC output) --> #72 (DiskQueue) --> #44 (Checkpointing)
                                           --> #45 (Backpressure)

#54 (Network sources) --> #35 (Per-input batching / SourceId)  [SOLVED BY SPEC]

#40 (Graceful shutdown) --> #75 (Shutdown flag bug)  [BOTH SOLVED BY SPEC]

#41 (Retry) --> #42 (HTTP timeouts)  [#41 solved, #42 partially]
            --> #72 (DiskQueue for retry buffer)  [SOLVED BY SPEC]

#36 (Enrichment JOINs) --> #37 (Background providers) --> #39 (geo_lookup UDF)

#44 (Checkpointing) --> #40 (Graceful shutdown)  [BOTH SOLVED BY SPEC]
                    --> #45 (Backpressure)  [SOLVED BY SPEC]

#53 (New sinks) --> #41 (Retry)  [SOLVED BY SPEC]
                --> #30 (Arrow Flight sink)

#47 (Configurable tunables) --> #28 (Adaptive batch sizing)  [BOTH SOLVED BY SPEC]

#76 (Scanner contract) --> #78 (Scanner-DF testing) --> #79 (Scanner-sink testing)
```

**Critical path for spec implementation:**
1. Phase 1: #35 (SourceId) + #75 (shutdown fix via CancellationToken) + #40 (graceful shutdown)
2. Phase 2: #71 (Arrow IPC) -> #72 (DiskQueue) + #44 (checkpointing)
3. Phase 3: #45 (backpressure via bounded channels) + #47 (PipelineRuntimeConfig)
4. Phase 4: #41 (retry via Sink SendResult) + #54/#53 (new sources/sinks using traits)

---

## Key Findings

1. **The spec directly solves 8 of 41 open issues** -- the core production blockers
   (#40, #41, #44, #45, #47, #72, #75) plus the design issue (#35).

2. **14 issues are partially addressed** -- the spec provides the trait/contract
   framework and these issues need implementation work within that framework.

3. **19 issues are fully independent** -- performance optimizations, UDFs, CI/CD,
   docs, and code quality work that can proceed in parallel with spec implementation.

4. **Zero conflicts detected.** No open issue contradicts a spec decision.

5. **The spec's biggest coverage gap is observability.** Issue #46 (structured
   logging) and the metrics the spec defines are complementary but the spec does not
   address the tracing/logging infrastructure itself.

6. **Issue #71 (Arrow IPC from StorageBuilder) is a spec prerequisite.** The DiskQueue
   component depends on compressed Arrow IPC serialization, which is not yet
   implemented. This should be prioritized ahead of Phase 2.

7. **The spec resolves the two open bugs** (#74 is independent, but #75 is directly
   fixed by CancellationToken shutdown).
