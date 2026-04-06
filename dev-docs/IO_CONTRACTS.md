# I/O Contracts

This document defines the current contract at the I/O boundaries:
receivers to pipeline,
pipeline to sinks,
checkpoint advancement,
error and retry vocabulary,
and file lifecycle duplicate/loss windows.

This is a living document.
It should reflect current repo truth first,
and future architecture changes should update it in the same PR that changes the behavior.

---

## How To Read This Doc

Each guarantee names:

- the boundary it applies to
- the current behavior or intended invariant
- the main enforcement mechanism
- important gaps or caveats

Enforcement labels:

| Label | Meaning |
|-------|---------|
| `Proof` | Enforced by Kani or model checking |
| `Proptest` | Enforced by property-based tests |
| `Integration test` | Enforced by deterministic integration or end-to-end tests |
| `Runtime check` | Enforced by code path checks or bounded behavior in production code |
| `Documented intent` | Desired contract called out in docs, but not yet strongly enforced |
| `TODO` | Not yet adequately enforced |

When a contract depends on another document,
this file links to the canonical source instead of restating the internals.

---

## Receiver -> Pipeline

### Boundary summary

Receivers accept external input,
normalize or decode it into pipeline-facing events,
and push those events into bounded internal channels.

The receiver boundary is responsible for:

- request validation and decoding
- body-size and queue-pressure handling
- producing decoded records or batches without silently dropping accepted data
- making backpressure visible to the sender

The pipeline boundary is not responsible for reconstructing malformed protocol payloads.

### Guarantees

| Guarantee | Enforcement | Notes |
|-----------|-------------|-------|
| Receivers use bounded channels rather than unbounded internal queues. | `Runtime check` | Queue-full behavior should surface as an explicit response rather than silent buffering. |
| Queue backpressure is visible to the sender. | `Runtime check` | Current receivers return non-success status codes when the pipeline side is full or disconnected. |
| OTLP requests are decoded before pipeline handoff. | `Integration test` | See receiver tests and transport E2E coverage in `crates/logfwd-io/tests/it/`. |
| Receiver shutdown must not rely on process exit to flush state safely. | `Documented intent` | Current behavior is uneven across receivers and is one reason `#1306` exists. |
| OTLP, OTAP, and Arrow IPC should ultimately share one transport/lifecycle substrate. | `Documented intent` | Architecture target tracked by `#1311`, not current-master truth. |
| Receivers should expose enough health and metrics to diagnose decode, queue, and shutdown problems. | `TODO` | Tracked by `#1306` and `#1311`. |

### Non-guarantees

- This document does not promise that all receivers already participate in the same input architecture.
- This document does not promise that all receivers already share one server implementation.
- This document does not claim protocol-level equivalence against external reference implementations yet.

---

## Pipeline -> Sink

### Boundary summary

The pipeline hands sinks a `RecordBatch` plus `BatchMetadata`.
The sink boundary decides whether delivery succeeded,
should be retried,
or should be treated as permanently rejected.

This boundary is the core place where output correctness and checkpoint semantics meet.

### Guarantees

| Guarantee | Enforcement | Notes |
|-----------|-------------|-------|
| Sinks receive `RecordBatch` plus delivery metadata instead of re-reading input state. | `Runtime check` | This is the active interface shape in current output paths. |
| A batch is considered in-flight once it begins send and remains so until ack processing resolves it. | `Runtime check` | This is encoded in pipeline lifecycle state and worker/pipeline interaction. |
| Sink delivery outcomes influence checkpoint advancement. | `Runtime check` | See `dev-docs/DESIGN.md` and `crates/logfwd-types/src/pipeline/`. |
| Fanout outcome is aggregated across child sinks. | `Runtime check` | Current aggregation exists, but the final contract is still evolving under `#1312`. |
| Output semantics should be explicit enough that new sink implementations must fit the contract rather than redefine it accidentally. | `Documented intent` | This is the desired architecture direction for `#1312`. |

### Non-guarantees

- This document does not claim transactional atomic commit across independent remote sinks.
- This document does not claim that current fanout semantics are already the final desired semantics.
- This document does not claim that all sink HTTP classification logic is already unified.

---

## Checkpoint Rules

Checkpoint behavior is one of the most important architecture contracts in the repo.

The canonical design background lives in [DESIGN.md](DESIGN.md).
This section captures the current boundary contract that implementation and tests rely on.

### Current rules

| Rule | Enforcement | Notes |
|------|-------------|-------|
| Checkpoints are opaque at the pipeline level. | `Runtime check` | The pipeline carries checkpoint type `C` without interpreting it. |
| Checkpoints advance through ack application, not by direct sink mutation. | `Runtime check` | See pipeline lifecycle code in `crates/logfwd-types/src/pipeline/`. |
| Rejected batches currently advance the checkpoint. | `Integration test` | This is current-master truth and is documented in [DESIGN.md](DESIGN.md). |
| Per-source checkpoint progress is independent. | `Runtime check` | Slow or rejected progress on one source should not inherently block another source's committed offset. |
| Failed or retried work remains part of the pipeline lifecycle until explicitly resolved. | `Runtime check` | This is part of the in-flight ticket model. |

### Architecture gaps

| Gap | Enforcement | Notes |
|-----|-------------|-------|
| Whether rejected batches should continue to advance checkpoints in the final architecture. | `TODO` | This is the central semantic redesign question in `#1312`. |
| Exact checkpoint behavior under final fanout contract. | `TODO` | Must be settled alongside exporter and fanout semantics. |
| End-to-end checkpoint guarantees for future file appender flows. | `TODO` | To be defined as `#1313` and `#1314` mature. |

### Duplicate and loss implications

Current-master implication:

- successful delivery can advance checkpoints
- rejected delivery can also advance checkpoints
- therefore permanently rejected data is currently treated as lost rather than replayable

This is current truth,
not a promise that the final architecture will keep this rule.

---

## Error And Retry Vocabulary

### Current vocabulary

Current code uses a mix of boundary-specific result types rather than one universal I/O algebra.

| Surface | Current vocabulary | Enforcement | Notes |
|---------|--------------------|-------------|-------|
| Output delivery | `SendResult` | `Runtime check` | Core sink delivery outcome surface today. |
| Pipeline lifecycle | ack / reject / fail transitions | `Runtime check` | These transitions are encoded in ticket and lifecycle types. |
| Receivers | receiver-specific HTTP or decode responses | `Runtime check` | Not yet fully unified across all receiver kinds. |

### Contract direction

| Direction | Enforcement | Notes |
|-----------|-------------|-------|
| Retryable vs permanent vs fatal distinctions should be explicit at the boundary. | `Documented intent` | Shared direction from the architecture umbrella. |
| Retry behavior should be bounded and observable. | `Documented intent` | Active architecture goal under `#1312`. |
| Shared classifier/helpers may exist where they clarify semantics rather than hide them. | `Documented intent` | Prior design input from closed foundation work, but not yet a final repo-wide truth. |

### Non-guarantees

- This document does not claim one fully unified error enum already exists across receivers, pipeline, and outputs.
- This document does not claim current retry behavior is already the final desired architecture.

---

## File Lifecycle Contract

The tailer/framing boundary is central to the repo's correctness story.
Scanner-specific parsing guarantees are documented in [SCANNER_CONTRACT.md](SCANNER_CONTRACT.md).
This section focuses on file and framing lifecycle behavior.

### Current file/tail lifecycle guarantees

| Guarantee | Enforcement | Notes |
|-----------|-------------|-------|
| File lifecycle events matter to downstream framing state. | `Runtime check` | Tail events such as truncation, rotation, and EOF are used to manage downstream partial-line state. |
| Per-source framing remainder is isolated by source ID. | `Runtime check` | Implemented in `crates/logfwd-io/src/framed.rs`. |
| EOF can flush a partial remainder. | `Integration test` | Covered by framed-input tests. |
| Truncation and rotation semantics are part of the downstream contract, not just tailer internals. | `Integration test` | Current tests exercise ordering-sensitive behavior. |
| File identity uses `(dev, inode, fingerprint)`-style logic in current code paths. | `Runtime check` | Exact policy is part of the tailer architecture track. |
| Read behavior is bounded per poll. | `Runtime check` | Current tailer code enforces bounded reads, though final fairness policy is still evolving. |

### Architecture gaps

| Gap | Enforcement | Notes |
|-----|-------------|-------|
| Final explicit state model for discovered/active/draining/evicted files. | `TODO` | Tracked by `#1310`. |
| Final fairness contract across many active files. | `TODO` | Current bounded reads help, but do not fully define fairness. |
| Quantified duplicate/loss window claims for every rotation/truncation/restart path. | `TODO` | Must only be tightened as tests and verification justify it. |

### Duplicate/loss windows

Current repo truth:

- duplicate and loss behavior depends on the specific file lifecycle path
- the repo has targeted regression tests around truncation, rotation, EOF, and framing remainder handling
- the repo does not yet have one final quantified duplicate/loss table for all file lifecycle scenarios

Therefore:

- do not claim stronger duplicate/loss bounds than the implementation and tests justify today
- update this section as `#1310`, `#1303`, and `#1314` tighten the guarantees

---

## Verification Mapping

This architecture relies on multiple verification layers.
The canonical strategy lives in [VERIFICATION.md](VERIFICATION.md).
This section maps the I/O contracts to the expected enforcement style.

| Contract class | Preferred enforcement |
|----------------|-----------------------|
| Pure helper invariants and local state transitions | Kani |
| Stateful boundary sequences with many interleavings | proptest and deterministic regression tests |
| Protocol/lifecycle rules independent of implementation shape | TLA+ |
| Distributed, crash, timeout, and retry fault scenarios | Turmoil |
| Wire-level OTLP semantic equivalence | loopback tests and oracle checks |
| Performance goals | disciplined benchmark validation |

This table is normative for planning,
not a claim that every item is already complete on `master`.

---

## Update Rules

- Any PR that changes receiver, pipeline, sink, checkpoint, or file lifecycle semantics should update this document in the same PR.
- If a stronger guarantee is not yet enforced, mark it as `Documented intent` or `TODO` rather than overstating certainty.
- If a more detailed subsystem contract already has a canonical document, link to it instead of duplicating it here.
