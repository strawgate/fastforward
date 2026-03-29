# Investigation Summary

All 6 research agents completed. Results in this directory.

## Verdicts

| Investigation | Verdict | Action |
|--------------|---------|--------|
| **Arrow IPC (DiskQueue)** | **GO** — feasible, bit-identical roundtrip, zstd supported, ~1M rows/sec | Proceed with Phase 0 spike (#85) |
| **SIMD scanner (#23)** | **SKIP** — stage 1 is 1% of pipeline time. Already simdjson-style on aarch64 | Add x86 SIMD to ChunkIndex (~50 LOC). Wire StreamingSimdScanner default (#69) |
| **Async migration** | **GO with caution** — incremental path works. Critical prereq: refactor SqlTransform first | Phase 2: Transform async (#88) MUST precede Phase 3: Pipeline orchestrator (#89) |
| **Columnar OTLP (#27)** | **SKIP full rewrite** — 15-20% gain, below 30% threshold | Quick win: extract column role resolution out of per-row loop (~30 min) |
| **String interning (#25)** | **DEFER** — field index already cached across batches. ~9% of batch cycle | Do target-cpu + PGO first. Try FxHashMap if profiling still shows HashMap |
| **PGO (#26)** | **DO NOW** — 8-12% throughput, zero code changes, just CI config | Add to nightly bench workflow. Also set target-cpu=x86-64-v3 |
| **io_uring (#31)** | **SKIP** — ~800 reads/sec, syscall overhead negligible. Incompatible with tokio | Revisit only at 500+ concurrent files |
| **Hyperscan (#29)** | **SKIP** — single-pattern use case, regex crate already SIMD-accelerated, adds C++ dep | Cache compiled regexes in UDF invoke (trivial fix) |

## Quick Wins Identified (can do immediately)

1. **target-cpu=x86-64-v3** in CI release builds — free AVX2, ~2-5% (trivial)
2. **PGO in nightly bench** — 8-12% throughput, ~30 lines of YAML (#26)
3. **Extract OTLP column roles out of per-row loop** — captures 80% of #27's gains (~30 min)
4. **Cache compiled regexes in grok/regexp_extract UDFs** — fixes per-batch recompilation (trivial)
5. **Wire StreamingSimdScanner as default** (#69) — 20% scan speedup, already built
6. **Add x86 ChunkIndex SIMD** — ~50 lines of SSE2/AVX2 intrinsics

## Critical Path Clarification

The async migration research revealed an important ordering constraint:

```
Phase 1: Source trait + Sink trait (parallel, no async yet)
    ↓
Phase 2: Transform async (#88) ← MUST come before Phase 3
    ↓   (also parallel: DiskQueue, CheckpointStore, Signal handling)
    ↓
Phase 3: Pipeline orchestrator (#89) — async task graph
```

**Why Transform first:** Current SqlTransform creates a tokio runtime per batch. If the pipeline goes async (Phase 3) while Transform still does this, calling `block_on()` inside a tokio worker panics with "cannot start runtime within runtime." Transform must be refactored to accept an external runtime handle BEFORE the pipeline becomes async.

## Issues to Update Based on Findings

| Issue | Update |
|-------|--------|
| #23 | Add comment: premature optimization, stage 1 is 1% of time. Quick win: add x86 SIMD to ChunkIndex |
| #27 | Add comment: full rewrite not worth it. Quick win: extract column roles out of per-row loop |
| #31 | Add comment: skip — syscall overhead negligible, incompatible with tokio |
| #29 | Add comment: skip — single-pattern, regex crate competitive, adds C++ dep. Quick win: cache compiled regexes |
| #25 | Add comment: defer — field index already cached. Try FxHashMap after PGO |
| #26 | Ready to implement — add target-cpu + PGO to nightly bench workflow |
| #69 | Ready to implement — just wire StreamingSimdScanner in pipeline.rs |

## Dependency on tokio-util

CancellationToken requires `tokio-util` which is NOT currently in the dep tree. Needs to be added in Phase 2.

## Dependency on reqwest

Current HTTP sinks use blocking `ureq`. For async Sink trait, should switch to `reqwest` which is already a transitive dep via `opentelemetry-otlp`. This should happen in Phase 1 (Sink trait).
