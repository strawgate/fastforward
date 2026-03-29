# Issue Triage & Organization Plan

Based on code audit, architecture spec validation, and dependency analysis.

## Codebase Reality Check

**What works today:**
- File input → CRI/JSON/Raw parsing → SIMD scanner → SQL transform → OTLP/HTTP/stdout output
- Single-threaded pipeline with 4MB/100ms batching
- Diagnostics server with Prometheus + JSON API
- CLI with --help, --version, --config, --validate, --dry-run, --check

**What's stubbed but not working:**
- UDP/TCP/OTLP inputs (config declares, pipeline rejects)
- Elasticsearch/Loki/Parquet/FileOut outputs (stubs with TODO, not wired into factory)
- storage.data_dir and server.log_level config fields (parsed, never used)

**What's built but not wired:**
- StreamingSimdScanner (zero-copy, 20% faster, not default in pipeline)

**What doesn't exist:**
- Disk queue, checkpointing, retry, backpressure, signal handling
- SQL rewriter (users must write level_str not level)
- Network input implementations
- Structured logging

## Issues to Close (subsumed by architecture spec)

| Issue | Reason |
|-------|--------|
| #47 | PipelineRuntimeConfig in spec exposes all tunables |
| #28 | Same — batch_target_bytes and batch_timeout are configurable |

## Issues to Update (add "architecture" label, reference spec)

These stay open as implementation tracking but should reference the spec component:

| Issue | Spec Component | Phase |
|-------|---------------|-------|
| #75 | Pipeline (CancellationToken) | Phase 2 signal handling |
| #40 | Pipeline (shutdown drain) | Phase 2 signal handling |
| #72 | DiskQueue | Phase 2 |
| #44 | CheckpointStore | Phase 2 |
| #45 | Pipeline (bounded channels) | Phase 3 |
| #41 | Sink (SendResult::RetryAfter) | Phase 1 Sink trait |
| #42 | Sink (async, internal timeouts) | Phase 1 Sink trait |
| #35 | Source (SourceId) | Phase 1 Source trait |
| #54 | Source implementations | Phase 4 |
| #53 | Sink implementations | Phase 4 |
| #71 | DiskQueue (Arrow IPC building block) | Phase 0 spike |

## New Issues to Create

| Title | Phase | Description |
|-------|-------|-------------|
| SQL rewriter: user-friendly SQL → typed-column SQL | Phase 3 | Users write `level = 'ERROR'`, rewriter produces `level_str = 'ERROR'`. Priority 2 from TODO.md but no tracking issue exists. |
| Phase 0: DiskQueue spike — Arrow IPC roundtrip benchmark | Phase 0 | Validate Arrow IPC write/read throughput. Can we sustain 1M+ lines/sec? Validate mmap vs sequential read. |
| Phase 1: Define Source trait + adapt FileInput | Phase 1 | Merge InputSource + FormatParser into unified Source. FileInput becomes first impl. |
| Phase 1: Define async Sink trait + adapt existing sinks | Phase 1 | Replace sync OutputSink with async Sink + SendResult. |
| Phase 2: Make SqlTransform async with shared runtime | Phase 2 | Stop creating tokio runtime per batch. Share pipeline's runtime. |
| Phase 3: Async Pipeline orchestrator with bounded channels | Phase 3 | Replace sync poll loop with tokio task graph. |

## Issues That Need Research/Prototyping First

| Issue | What to validate | Effort | Decision point |
|-------|-----------------|--------|----------------|
| #23 SIMD scanner | Benchmark current scanner vs simdjson-rs. Is there a measurable gap? | 2-3 days | If <20% gap, skip. If >2x gap, invest. |
| #31 io_uring | Compare throughput on 100+ concurrent files vs current notify+read. | 2-3 days | If <30% improvement, skip. Linux-only. |
| #27 columnar OTLP | Encode 8K-row batch row-at-a-time vs column-at-a-time. | 1-2 days | If <30% speedup, skip. |
| #29 Hyperscan | Benchmark grok() with Hyperscan vs regex crate on 10 real patterns. | 1-2 days | If <3x speedup, skip (adds C++ dep). |
| #25 string interning | phf vs HashMap vs linear scan on 10-20 field names. | Half day | Quick prototype. |
| #24 zstd dictionary | Train on 1M real logs, compare ratio vs vanilla zstd level 1. | Half day | If <10% improvement, skip. |
| #26 PGO | Just do it — CI config change, measure before/after. | 1 day | Always worth it. |

## Issues That Are Independent (no architecture dependency)

Can be done anytime by anyone:

| Issue | Category |
|-------|----------|
| #74 | Bug: diagnostics bind panic |
| #48 | Code quality: remove unwrap/expect |
| #46 | Observability: structured logging with tracing |
| #67 | Code quality: clippy pedantic lints |
| #34 | Config: typed input/output config |
| #43 | Deploy: K8s DaemonSet manifest |
| #49 | CI: Docker, security, platforms |
| #50 | CI: release automation |
| #51 | Testing: integration test suite |
| #52 | Docs: config reference, guides |
| #69 | Perf: wire StreamingSimdScanner (after #76-79) |
| #70 | Perf: dictionary encoding in StorageBuilder |
| #76-79 | Testing: scanner boundary tests |
| #77 | Testing: fuzz SIMD scanner |
| #36, #37 | Feature: enrichment via JOINs/UDFs |
| #39 | Feature: geo_lookup UDF |
| #30 | Research: Arrow Flight output |
| #32 | Research: containerd shim |

## Dependency Graph (simplified)

```
PHASE 0 (parallel, risk reduction):
  [Scanner boundary tests #76-79]    [DiskQueue spike]    [PGO #26]

PHASE 1 (parallel, trait definitions):
  [Source trait]    [Sink trait]    [StreamingSimdScanner default #69]

PHASE 2 (parallel, async internals):
  [Transform async]    [DiskQueue #72]    [CheckpointStore #44]    [Signal handling #40+#75]

PHASE 3 (critical path):
  [Pipeline orchestrator #45]    [SQL rewriter]

PHASE 4 (parallel, feature expansion):
  [TCP Source]  [UDP Source]  [OTLP Source]  [ES Sink]  [Loki Sink]  [File/Parquet Sink]
```

## TODO.md Status

| Priority | Status | Notes |
|----------|--------|-------|
| P1: Wire v2 pipeline | DONE | pipeline.rs exists and works |
| P2: SQL rewriter | NOT DONE | No issue exists — create one |
| P3: Benchmark v2 path | PARTIALLY DONE | Competitive benchmarks exist but don't isolate stages |
| P4: Input abstraction | PARTIALLY DONE | InputSource trait exists, only FileInput impl |
| P5: E2E cursor tracking | NOT DONE | Architecture spec covers this as CheckpointStore |
| P6: New CLI modes | DONE | --config, --validate, --dry-run, --check all work |

TODO.md should be updated to point to the architecture spec and issue tracker.
