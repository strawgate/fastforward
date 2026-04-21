# Kafka Source Contract Research (Issue #1475)

> **Date:** 2026-04-21
> **Status:** Active
- **Authoring mode:** Design research only (no implementation)

## 0) Scope notes and audit reality check

This report was requested to audit specific `logfwd` source files, especially:

- `crates/logfwd-io/src/input.rs`
- existing source implementations (file/TCP/UDP/OTLP/generator)
- `examples/use-cases/kafka-to-otlp.yaml`

During this run, those paths are **not present** in the checked-out workspace. That prevents direct validation of concrete trait signatures and current checkpoint plumbing.

Therefore, this report provides:

1. A **proposed contract** aligned to the terms in the request (`InputSource`, `InputEvent`, `SourceId`, `ByteOffset`).
2. A Kafka design that is robust under typical pull-based source loops.
3. Explicit assumptions where direct code confirmation was not possible.

> **Action for implementation phase:** before coding, re-run this against the repo/branch that contains `crates/logfwd-io` and reconcile naming/signatures 1:1.

---

## 1) Rust Kafka crate evaluation matrix

### Quick recommendation

**Primary recommendation: `rdkafka`** (with constrained feature set) for production Kafka source support.

Reason: it is the only option among the three candidates that cleanly supports mature consumer groups, rebalance handling, offset commit controls, SASL/TLS coverage, and broad Kafka compatibility.

### Matrix

| Criterion | `rdkafka` (librdkafka binding) | `rskafka` (pure Rust) | `kafka-rust` (older pure Rust) |
|---|---|---|---|
| Consumer group support | **Yes**, mature | **No** (explicitly out of scope) | Partial/legacy APIs |
| Rebalance callbacks / assignment control | **Yes** | N/A (no CG support) | Limited/older model |
| Offset commit semantics | **Strong** (manual commit supported) | App-managed offsets only | Basic support |
| Throughput/latency maturity | **High** (librdkafka) | Good for narrow use cases | Lower confidence |
| Kafka feature completeness | **High** | Intentionally minimal | Older protocol coverage |
| Dependency model | C dependency (librdkafka) but static link options exist | Pure Rust | Pure Rust |
| Operational risk | **Low** (widely used) | Medium (feature gaps for this use case) | Medium-high (age/maintenance risk) |
| Fit for this source (group consumer + checkpoint integration) | **Best fit** | Not suitable without major custom work | Not preferred |

### Philosophy fit: “static-binary, minimal dependency”

- If strict “no C deps ever” is a hard requirement, none of the mature group-consumer options are ideal.
- For practical production ingestion, `rdkafka` can still be aligned with static deployment by compiling bundled `librdkafka` and minimizing optional features.
- `rskafka` is philosophically cleaner (pure Rust) but lacks required consumer group machinery for the requested contract.

### Suggested dependency posture

Use `rdkafka` with:

- `default-features = false` where possible
- enable only needed transport/security/compression features
- pinned version and explicit build notes in docs/CI
- smoke tests on target distros/containers for static artifact reliability

---

## 2) Consumer group design

## 2.1 Group identity model

**Recommendation:**

- **Default group id scope: one consumer group per pipeline**.
- Group id template: `logfwd.{pipeline_id}.{source_id}` (or equivalent deterministic identity).

Why:

- Cleanly maps ownership and lag to one pipeline.
- Avoids cross-pipeline interference when multiple pipelines read the same topic set.
- Simplifies incident response (“which pipeline owns this lag?”).

Alternative (single group per instance) is weaker because it couples unrelated pipelines and complicates independent scaling.

## 2.2 Membership and assignment

- Use Kafka automatic group management and cooperative assignor when available.
- Do **not** hand-roll manual static partition maps for first release.
- Expose optional `group_instance_id` for sticky static membership in stable deployments.

## 2.3 Rebalance lifecycle contract

Define three explicit phases in source runtime:

1. **Before revoke**: stop pulling new records for partitions being revoked.
2. **Revoke barrier**: flush/ack in-flight events for revoked partitions, then commit offsets up to durable checkpoint point.
3. **After assign**: seek each newly assigned partition from resolved start offset policy (checkpoint > committed > reset policy).

This prevents committing offsets for events not yet durably accepted by downstream pipeline.

## 2.4 Concurrency model

- Single consumer instance per source, polled on source task loop.
- Partition-aware in-flight tracking keyed by `(topic, partition)`.
- Backpressure gating: if downstream queue is saturated, pause partitions rather than continue fetching.

---

## 3) Checkpoint and offset integration design

## 3.1 Mapping Kafka offsets to `SourceId` / `ByteOffset`

Because Kafka has tuple coordinates, use:

- `SourceId = kafka:{cluster_alias}:{topic}:{partition}`
- `ByteOffset` stores Kafka **message offset** (logical offset), not byte position.

Notes:

- Name `ByteOffset` is semantically overloaded for Kafka; document that for non-byte-addressable sources it means “monotonic source position”.
- Commit value should represent “next offset to read”, i.e. `last_processed_offset + 1`.

## 3.2 Checkpoint write timing

**Recommended:** manual offset management tied to logfwd checkpoint durability.

Flow:

1. Poll records.
2. Emit `InputEvent`s into pipeline and mark them in-flight.
3. When checkpoint flush confirms durable acceptance, advance per-partition high-watermark.
4. Commit Kafka offsets for only those durable watermarks.

Avoid Kafka auto-commit; it can acknowledge messages before pipeline durability, causing loss on crash.

## 3.3 Restart semantics

On restart per partition:

1. If local logfwd checkpoint exists, seek from checkpoint (authoritative).
2. Else if broker committed offset exists for group, start there.
3. Else apply `auto.offset.reset` policy (`earliest` default for safety).

This ordering keeps behavior deterministic even if broker and local checkpoint diverge.

## 3.4 Divergence handling

If local checkpoint and broker commit disagree:

- Prefer **local durable checkpoint** if it is within retention.
- If requested offset is out-of-range (aged out), emit clear error + metric and apply configurable fallback:
  - `fail`
  - `earliest`
  - `latest`

## 3.5 Exactly-once feasibility with this architecture

With source-only ingestion into logfwd pipeline (without transactional sink coupling), design target is:

- **At-least-once** end-to-end.

Exactly-once requires transactionally coupling consumed offsets with sink writes, which is usually sink-specific and outside basic source contract.

---

## 4) Payload and header mapping

## 4.1 Payload mode

Default source should emit raw bytes and avoid format lock-in.

- `payload: bytes` from Kafka record value.
- optional metadata field for key bytes.
- downstream processors/parsers handle JSON/Avro/Protobuf decode.

This keeps source generic and consistent with other transport sources.

## 4.2 Metadata mapping

Attach canonical Kafka metadata to each `InputEvent`:

- `kafka.topic`
- `kafka.partition`
- `kafka.offset`
- `kafka.timestamp` (type + millis if present)
- `kafka.key` (optionally encoded)
- `kafka.headers.*` (header map)
- `kafka.leader_epoch` (if exposed)

Headers:

- Preserve duplicates by storing as multi-value list per key.
- Preserve binary values (do not force UTF-8).

## 4.3 Topic → pipeline routing

Two viable modes:

1. **Single-source single-pipeline** (v1 default): source subscribes to configured topics and forwards to owning pipeline.
2. **Pattern fanout** (later): one source reads topic regex and routes by matching rules.

For first implementation, keep routing simple and explicit in pipeline config.

---

## 5) Config schema proposal (`KafkaInputConfig`)

```yaml
input:
  type: kafka
  brokers: ["kafka-1:9092", "kafka-2:9092"]
  topics: ["logs.app", "logs.audit"]        # mutually exclusive with topic_pattern
  topic_pattern: null                           # optional regex

  group_id: null                                # default derived from pipeline/source
  client_id: "logfwd"
  group_instance_id: null                       # optional static membership

  auto_offset_reset: earliest                   # earliest|latest|none
  session_timeout_ms: 45000
  heartbeat_interval_ms: 3000
  max_poll_interval_ms: 300000

  fetch_max_bytes: 52428800
  max_partition_fetch_bytes: 1048576
  queued_min_messages: 100000

  enable_auto_commit: false                     # forced false in v1
  commit_interval_ms: 5000                      # cadence for checkpoint-driven commit attempts

  security:
    protocol: plaintext                         # plaintext|ssl|sasl_plaintext|sasl_ssl
    ssl:
      ca_file: null
      cert_file: null
      key_file: null
      key_password: null
    sasl:
      mechanism: null                           # plain|scram_sha_256|scram_sha_512|gssapi|oauthbearer
      username: null
      password: null

  start_position:
    prefer_local_checkpoint: true
    fallback: committed                         # committed|earliest|latest
    out_of_range: fail                          # fail|earliest|latest

  metadata:
    include_key: true
    include_headers: true
    include_timestamp: true

  backpressure:
    pause_on_queue_full: true
    resume_threshold_ratio: 0.5

  observability:
    emit_lag_metrics: true
    emit_rebalance_events: true
```

Validation rules:

- exactly one of `topics` or `topic_pattern` must be set.
- if SASL configured, require matching protocol/mechanism fields.
- `enable_auto_commit` should be rejected if `true` (v1 safety rule).

---

## 6) Implementation plan and effort estimate

Assumes existing `InputSource` abstractions and checkpoint store already exist.

## Phase 0 — Contract alignment (0.5–1 day)

- Reconcile actual trait signatures in `crates/logfwd-io/src/input.rs`.
- Confirm event metadata envelope and checkpoint API shape.
- Produce short ADR confirming offset model.

## Phase 1 — Basic source skeleton (1.5–2.5 days)

- Add `KafkaInputConfig` and validation.
- Add source constructor + poll loop scaffold.
- Subscribe to topics and emit bytes payload events with metadata.

## Phase 2 — Checkpoint-coupled commits (2–3 days)

- Partition in-flight tracker.
- Durable-watermark advancement on checkpoint flush callback.
- Manual commit logic + restart seek precedence.

## Phase 3 — Rebalance correctness (1.5–2.5 days)

- Revoke/assign handlers with flush barrier.
- Partition pause/resume coordination.
- Recovery tests for rebalance churn.

## Phase 4 — Hardening + observability (1.5–2 days)

- Metrics: lag, commit latency, commit failures, rebalance counters.
- Structured logs for offset decisions.
- Failure injection tests (broker bounce, retention gap, crash/restart).

## Total estimate

- **~7 to 11 engineer-days** for production-ready v1.
- Additional **2–4 days** if TLS/SASL matrix and CI integration are expanded immediately.

---

## 7) Delivery guarantee analysis

## 7.1 At-most-once

Possible only if committing before processing; not recommended (data loss risk).

## 7.2 At-least-once (recommended baseline)

Achieved by:

- processing record → durable checkpoint → commit offset
- replaying uncommitted records on crash

Tradeoff: duplicates are possible; downstream idempotency strongly recommended.

## 7.3 Exactly-once

Not realistic in generic source-only contract unless:

- sink supports transactions/idempotent writes, and
- consumed offsets are atomically committed with sink transaction outcome.

This is typically pipeline/sink-specific, not a plain input-source guarantee.

## 7.4 Failure scenario summary

- Crash before checkpoint flush: record replayed (duplicate possible, no loss).
- Crash after checkpoint flush but before commit: replay from older commit unless local checkpoint precedence used.
- Rebalance during processing: revoke barrier required to avoid committing unflushed records.
- Retention truncation: out-of-range fallback policy decides fail-fast vs rewind/skip.

---

## 8) Key decisions to ratify before implementation

1. Confirm `rdkafka` acceptance despite C dependency.
2. Confirm group id default scope (per pipeline/source).
3. Confirm local checkpoint precedence over broker committed offset.
4. Confirm v1 payload contract is raw bytes + metadata only.
5. Confirm at-least-once as explicit non-EOS target for v1.

---

## 9) External references (primary)

- rust-rdkafka docs (`rdkafka`): feature/semantics and delivery guidance.
- rdkafka-sys docs: static/dynamic linking behavior and build features.
- rskafka docs: explicit non-goal for consumer groups/offset tracking.
- kafka-rust repository README/API notes for group-offset support and caveats.

(Implementation phase should pin exact crate versions in `Cargo.toml` and revalidate capabilities at that commit.)
