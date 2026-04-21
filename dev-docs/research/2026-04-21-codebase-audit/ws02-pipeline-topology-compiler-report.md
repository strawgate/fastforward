# Pipeline Topology Compiler Design Report (WS02)

- **Issue:** #1363
> **Status:** Research proposal
> **Date:** 2026-04-21
- **Authoring mode:** Codebase-audit follow-up (design only; no runtime code changes)

---

## 0) Audit findings and assumptions

### Requested code paths were not present in this checkout

This workspace does **not** include the Rust paths referenced in the issue prompt (for example `crates/logfwd-runtime/src/pipeline/*.rs` and `crates/logfwd-config/src/types.rs`). I validated this by searching the repository and checking for the exact files/trees.

Because those files are absent in the current checkout, the design below is based on:

1. The issue context you provided (imperative I/O+CPU split already exists, no declarative topology yet).
2. Existing repository architecture docs and prior fanout reports to keep style and implementation planning consistent.

### Practical implication

This report proposes a concrete compiler/runtime contract that can be dropped into the intended `logfwd-runtime` / `logfwd-config` crates, but exact type names and signatures should be reconciled against the actual Rust sources once that subtree is available.

---

## 1) Architecture proposal

### 1.1 What the compiler consumes

The compiler should consume a **pipeline intent model** (declarative config) rather than executable worker wiring. The config should represent:

- Inputs (source type + scan/tailer settings)
- Processor chain (ordered transforms/filters)
- Optional SQL transform stage
- Outputs/sinks
- Delivery semantics controls (ack/checkpoint policy)
- Resource hints (parallelism, batching, queue bounds)

Proposed entrypoint (conceptual):

```rust
fn compile_topology(spec: PipelineSpec, ctx: CompileContext) -> Result<CompiledTopology, CompileReport>
```

Where:

- `PipelineSpec`: user config (single pipeline)
- `CompileContext`: catalogs/capabilities (available processors, sink capabilities, feature flags)
- `CompiledTopology`: normalized executable plan
- `CompileReport`: structured diagnostics for `--dry-run` and runtime startup

### 1.2 What the compiler produces

The compiler should output a **CompiledTopology** that `run()` can consume directly, replacing hardcoded imperative wiring.

`CompiledTopology` should include:

1. **Graph IR**: typed DAG of stages and channels.
2. **Execution partitions**:
   - I/O partition (source readers, output writers)
   - CPU partition (scan parse, processors, SQL, aggregation)
3. **Checkpoint plan**:
   - checkpoint domains
   - boundary rules
   - exactly where acknowledgements can be emitted
4. **Resource plan**:
   - channel capacities
   - worker counts
   - batching constraints
5. **Validation metadata**:
   - warnings, inferred defaults, capability downgrades

### 1.3 Runtime integration model

Current `run()` loops should receive a single compiled object:

```text
PipelineManager::start(compiled_topology)
```

Runtime responsibilities become:

- Instantiate workers according to `ExecutionPlan`
- Materialize channels and backpressure policy from `ResourcePlan`
- Enforce checkpoint behavior from `CheckpointPlan`
- Emit topology-aware observability (per-node throughput/latency/errors)

Compiler responsibilities become:

- Structural validation
- Semantic validation (processor/sink compatibility)
- Constraint solving (checkpoint placement, partitioning, defaults)

---

## 2) Topology graph model

### 2.1 Node taxonomy

Use a typed DAG with stable node IDs:

- `InputNode` (tail/file/kafka/http/etc.)
- `ScanNode` (parse/decode/tokenize)
- `ProcessorNode` (AddField, Filter, Rename, Dedup, RateLimit, etc.)
- `SqlTransformNode` (optional)
- `OutputNode` (sink writer)
- `BarrierNode` (compiler-inserted checkpoint or repartition boundary)

### 2.2 Edge taxonomy

Edges need semantics, not just connectivity:

- `DataEdge` (event/batch flow)
- `ControlEdge` (heartbeat, shutdown, drain)
- `AckEdge` (downstream durability/commit signal traveling upstream)

For execution planning, each `DataEdge` should annotate:

- queue type (bounded/unbounded/priority)
- batch contract (record vs batch)
- ordering contract (preserve input order / partition order / unordered)

### 2.3 Checkpoint boundary inference

Checkpoint boundaries should be inferred from **durability + replayability** semantics.

Heuristic rules:

1. **Before non-replayable side effects** insert a boundary.
   - Example: sink that cannot deduplicate idempotently.
2. **After deterministic pure transforms** do **not** force boundary.
3. **At stateful processors** require explicit policy:
   - state snapshot frequency
   - rollback/recovery scope
4. **At fan-out/fan-in joins** create explicit barrier nodes to prevent ambiguous ack propagation.

Output artifact:

- `CheckpointPlan { domains: Vec<CheckpointDomain>, ack_routes: Vec<AckRoute> }`

This keeps ack/commit semantics auditable and testable.

### 2.4 Normal form constraints

Compiler should normalize all valid pipeline specs to a predictable form:

- exactly one entry node per pipeline
- no implicit cycles
- every output reachable from input
- every processor path type-checks (`EventSchema` compatibility)
- explicit barrier nodes wherever commit semantics change

---

## 3) `--dry-run` design

### 3.1 What `--dry-run` validates

`--dry-run` should run the **full compile pipeline** except worker startup.

Validation layers:

1. **Syntax/config validation**
2. **Graph structural validation** (DAG, reachability, fanout constraints)
3. **Capability validation**
   - processor exists
   - processor supported in selected runtime mode
   - sink supports requested guarantees
4. **Schema validation**
   - field references exist after prior transforms
   - SQL columns resolvable
5. **Checkpoint safety validation**
   - every ack path terminates in a commit-capable boundary
6. **Resource sanity checks**
   - queue bounds, worker count, impossible memory hints

### 3.2 Output/report format

`--dry-run` should emit:

- Human-readable summary table
- Machine-readable JSON report for CI

Suggested JSON shape:

```json
{
  "status": "ok|warn|error",
  "pipelines": [{
    "name": "ingest-main",
    "nodes": 12,
    "edges": 14,
    "checkpoint_domains": 2,
    "warnings": [],
    "errors": []
  }],
  "global_warnings": []
}
```

### 3.3 Exit semantics

- `0`: compile success (warnings allowed unless `--warnings-as-errors`)
- `1`: compile failure
- Optional `2`: invalid CLI usage

### 3.4 Visualization

Add optional:

- `--emit-dot <path>` Graphviz DOT
- `--emit-compiled-json <path>` normalized IR dump

This is high leverage for debugging and design reviews.

---

## 4) Multi-pipeline design (shared vs independent resources)

### 4.1 Control-plane model

Introduce a **TopologySet**:

```text
TopologySet = { pipelines: Vec<CompiledTopology>, shared_resources: SharedResourcePlan }
```

Each pipeline is independently compiled first, then a set-level planner resolves sharing.

### 4.2 Shared resources

Safe to share:

- Input watcher/scanner when source is identical and replay cursor is compatible
- Connection pools for same sink backend
- Processor registry and compiled expression caches
- Telemetry exporters and metric registries

Not safe to share by default:

- Checkpoint stores unless namespace-isolated
- Stateful processor instances (unless explicitly keyed/partitioned)

### 4.3 Isolation boundaries

Each pipeline should have:

- own checkpoint namespace
- own failure domain
- own backpressure accounting

So one pipeline stall should not globally deadlock others.

### 4.4 Shared input semantics

If two pipelines share one input stream, choose explicitly:

1. **Tee mode** (each pipeline gets full copy)
2. **Partition mode** (records split by key/rule)

Compiler must reject ambiguous default behavior here.

---

## 5) Processor chain composition model

### 5.1 Placement around scan and SQL

Recommended canonical order:

```text
Input -> Scan -> Pre-SQL processors -> SQL transform (optional) -> Post-SQL processors -> Output
```

Why split pre/post SQL:

- Pre-SQL: enrichment/filtering to reduce SQL workload
- Post-SQL: output shaping, dedup/rate limiting for sink semantics

### 5.2 Processor contract

Each processor should declare metadata:

- `purity`: pure | stateful
- `determinism`: deterministic | nondeterministic
- `schema_effect`: add/remove/rename/mutate fields
- `checkpoint_sensitivity`: none | requires_barrier_before | requires_barrier_after

Compiler can then enforce valid ordering and checkpoint insertion.

### 5.3 Simple processor composition

For `AddField`, `Filter`, `Rename`, `Dedup`, `RateLimit`:

- `AddField`, `Rename`, `Filter`: usually pure/deterministic
- `Dedup`: stateful, requires bounded window/state snapshot policy
- `RateLimit`: time/stateful, can drop or delay; must annotate downstream semantics

### 5.4 Stateful processors (e.g., tail sampling)

Stateful processors should run in CPU partition with explicit state backend options:

- in-memory (best effort)
- local durable state
- remote state store

Compiler should require recovery policy selection when stateful processors are present in at-least-once/exactly-once modes.

---

## 6) Implementation plan (ordered phases)

> File paths below target the intended Rust layout from the issue prompt.

### Phase 1 — IR and compiler scaffolding

- Add `crates/logfwd-runtime/src/pipeline/topology_ir.rs`
  - Node/edge definitions
  - `CompiledTopology`, `ExecutionPlan`, `CheckpointPlan`
- Add `crates/logfwd-runtime/src/pipeline/compiler.rs`
  - compile entrypoint
  - diagnostics model

### Phase 2 — Config to spec mapping

- Extend `crates/logfwd-config/src/types.rs`
  - declarative `PipelineSpec`
  - processor chain config
  - multi-pipeline top-level config
- Add config normalization pass (defaults and explicitness)

### Phase 3 — Validation and dry-run

- Add `crates/logfwd-runtime/src/pipeline/validate.rs`
- Wire CLI `--dry-run` to compile-only path
  - return compile diagnostics
  - optionally emit DOT/JSON

### Phase 4 — Runtime integration

- Refactor `crates/logfwd-runtime/src/pipeline/mod.rs`
  - construct from `CompiledTopology` instead of imperative wiring
- Refactor `crates/logfwd-runtime/src/pipeline/input_pipeline.rs`
  - replace direct channel setup with plan materialization
- Update run loop files (`run.rs`, `submit.rs`) to consume plan contracts

### Phase 5 — Checkpoint integration

- Enhance `checkpoint_policy.rs` with compiler-produced domains/routes
- Ensure drain/heartbeat semantics observe barrier nodes

### Phase 6 — Multi-pipeline orchestrator

- Add topology set planner and shared resource planner
- Introduce pipeline-level fault isolation and metric tagging

### Phase 7 — Processor registry and composition checks

- Add processor metadata/trait extensions for purity/state/schema effect
- Compile-time ordering checks
- Snapshot/recovery requirements for stateful processors

### Phase 8 — Test matrix

- Compiler unit tests (graph validity, checkpoint insertion)
- Golden tests for dry-run JSON
- Integration tests for multi-pipeline stall/failure isolation
- Replay tests for checkpoint correctness across crash/restart

---

## 7) Risks, open questions, dependencies

### 7.1 Risks

1. **Hidden semantics in existing imperative wiring** may be lost during refactor if not codified first.
2. **Checkpoint correctness drift** if barrier inference rules are too implicit.
3. **Operational complexity** from multi-pipeline shared resources (especially shared inputs).
4. **Processor state explosion** for dedup/rate-limit/sampling without strong cardinality controls.

### 7.2 Open questions

1. Target delivery guarantee by default: at-least-once or exactly-once?
2. Should SQL stage be mandatory-normalized as a processor node internally?
3. What is the minimal stable IR guaranteed for tooling (`--emit-compiled-json` consumers)?
4. How are versioned pipeline specs migrated across runtime upgrades?
5. Do we support cross-pipeline joins, or keep pipelines independent DAGs only?

### 7.3 Dependencies

- Processor metadata registry (for compile-time reasoning)
- Checkpoint store API with namespacing and recovery primitives
- CLI/reporting plumbing for dry-run and emitted artifacts
- Observability schema updates (topology/node IDs in metrics/logs)

---

## 8) Recommended next steps (short-term)

1. Write an ADR fixing IR vocabulary (`node`, `edge`, `barrier`, `domain`, `route`) before implementation.
2. Land compiler scaffold + dry-run first (no runtime behavior change).
3. Add “compile then execute” shim to current runtime for phased migration.
4. Gate multi-pipeline shared-input mode behind explicit config + validation errors by default.

This sequence minimizes regression risk while unlocking topology-level validation early.
