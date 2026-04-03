# Kani Verification Plan: Checkpoint-Remainder Coordination

Date: 2026-03-31
Context: Formally verify the pure logic that coordinates file read offsets,
newline framing remainders, and checkpoint persistence across crash/restart
cycles.

## Problem

The file tail pipeline has a subtle coordination problem: the byte offset
reported to the checkpoint store must account for the framing remainder.
If we checkpoint at `read_offset` (the position after the last OS read)
but the framer has buffered `remainder_len` unprocessed bytes, a crash
and restart from that checkpoint will skip the remainder -- data loss.

The correct checkpoint is `read_offset - remainder_len`, i.e., the byte
position of the last complete newline boundary that was fully processed.
This invariant is maintained across a sequence of Read, Frame, Checkpoint,
Crash, and Restart operations. Getting it wrong means either data loss
(checkpoint too far ahead) or duplicate processing (checkpoint too far
behind -- acceptable, but wasteful).

Today this logic is spread across `FramedInput` (remainder management),
`TailedFile` (offset tracking), and `PipelineMachine` (ordered ACK).
None of the coordination invariants are formally verified.

## What pure logic can we extract?

The checkpoint-remainder coordination is a pure state machine with no I/O
dependencies. We can extract it into `logfwd-core` as a `no_std`-compatible
module, following the same pattern as `pipeline/batch.rs` and `framer.rs`.

### State

```rust
/// Pure state machine for checkpoint-remainder coordination.
///
/// Tracks the relationship between file read position, framing
/// remainder, and the checkpoint that should be persisted.
/// All fields are byte offsets within a single file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointTracker {
    /// Total bytes read from the file (cumulative).
    /// Advances on every Read operation.
    read_offset: u64,

    /// Byte offset of the last complete newline boundary.
    /// This is where we would resume after a crash.
    /// Invariant: processed_offset <= read_offset
    processed_offset: u64,

    /// Number of bytes after the last newline that are buffered
    /// as a partial-line remainder.
    /// Invariant: processed_offset + remainder_len == read_offset
    remainder_len: u64,

    /// Last durably persisted checkpoint offset.
    /// Invariant: checkpoint_offset <= processed_offset
    checkpoint_offset: u64,
}
```

### Actions

```rust
/// Actions that drive the state machine.
#[derive(Debug, Clone, Copy)]
pub enum Action {
    /// Read n_bytes from the file. The framer found the last
    /// newline at position last_newline_pos within the chunk
    /// (None if no newline was found).
    Read {
        n_bytes: u64,
        last_newline_pos: Option<u64>,
    },

    /// Persist the current processed_offset as the checkpoint.
    Checkpoint,

    /// Simulate a crash: state is lost except checkpoint_offset.
    Crash,

    /// Restart from the last checkpoint: reset read state.
    Restart,
}
```

### Transitions

```rust
impl CheckpointTracker {
    /// Create a new tracker, optionally resuming from a checkpoint.
    pub fn new(resume_offset: u64) -> Self {
        CheckpointTracker {
            read_offset: resume_offset,
            processed_offset: resume_offset,
            remainder_len: 0,
            checkpoint_offset: resume_offset,
        }
    }

    /// Apply a Read action: we read n_bytes starting at read_offset.
    /// The framer scanned the chunk and found the last newline at
    /// last_newline_pos (relative to the start of the chunk).
    ///
    /// If last_newline_pos is None, the entire chunk is remainder
    /// (no complete lines).
    pub fn apply_read(&mut self, n_bytes: u64, last_newline_pos: Option<u64>) {
        let old_read = self.read_offset;
        self.read_offset = old_read + n_bytes;

        match last_newline_pos {
            Some(pos) => {
                // pos is relative to chunk start; +1 because the newline
                // itself is consumed (processed_offset is one past the \n).
                let newline_abs = old_read + pos + 1;
                self.processed_offset = newline_abs;
                self.remainder_len = self.read_offset - newline_abs;
            }
            None => {
                // No newline in chunk -- everything is added to remainder.
                self.remainder_len += n_bytes;
                // processed_offset unchanged.
            }
        }
    }

    /// Persist the current processed_offset as the durable checkpoint.
    pub fn apply_checkpoint(&mut self) {
        self.checkpoint_offset = self.processed_offset;
    }

    /// The offset that should be checkpointed.
    /// This is always at a newline boundary.
    pub fn checkpointable_offset(&self) -> u64 {
        self.processed_offset
    }

    /// Restart from the last checkpoint. Remainder is lost.
    pub fn apply_restart(&mut self) {
        self.read_offset = self.checkpoint_offset;
        self.processed_offset = self.checkpoint_offset;
        self.remainder_len = 0;
    }
}
```

## Invariants to verify

These are the properties that must hold after every action sequence:

1. **Offset ordering**: `checkpoint_offset <= processed_offset <= read_offset`
2. **Remainder consistency**: `processed_offset + remainder_len == read_offset`
3. **No data loss on restart**: After Crash + Restart, `read_offset == checkpoint_offset`,
   so no bytes between `checkpoint_offset` and the old `processed_offset` are skipped.
   (Bytes between `processed_offset` and old `read_offset` -- the remainder -- are
   re-read and re-framed, which is correct.)
4. **Checkpoint monotonicity**: `checkpoint_offset` never decreases.
5. **Processed offset monotonicity**: `processed_offset` never decreases
   (within a single file lifecycle; restart resets to checkpoint).

## Bounds analysis

Reference: `KANI_LIMITS.md` documents ~8 state transitions as the practical
bound for moderately complex state machines (s2n-quic uses `InlineVec<Op, 8>`
with `#[kani::unwind(9)]`).

Our state machine is simpler than s2n-quic's QUIC packet processing:
- 4 action variants (vs. s2n-quic's ~10+ operation types)
- 4 u64 state fields (pure arithmetic, no collections)
- No heap allocation, no BTreeMap, no dynamic dispatch

Estimated bounds:

| Depth | Actions explored | Solver time (est.) | Coverage |
|-------|-----------------|-------------------|----------|
| 4     | All 4-step sequences | < 10s | Basic crash-restart |
| 6     | All 6-step sequences | < 30s | Read-Read-Checkpoint-Read-Crash-Restart |
| 8     | All 8-step sequences | < 60s | Full cycle with retries |

The key insight: since our state is just 4 u64 fields with pure arithmetic,
Kani's SAT solver handles this efficiently. There are no loops over symbolic
data (unlike the framer proofs which iterate over byte arrays). The u64
values are symbolic but the operations are loop-free arithmetic, which is
Kani's sweet spot per KANI_LIMITS.md ("loop-free bitwise operations verify
over the full 64-bit range").

We should be conservative and start at depth 6, then push to 8 if solver
time permits. The unwind bound is `depth + 1` for the action loop.

## Concrete Kani harnesses

### File location

New file: `crates/logfwd-core/src/checkpoint_tracker.rs`

Expose from `crates/logfwd-core/src/lib.rs` as `pub mod checkpoint_tracker;`.

This follows the existing pattern: pure logic in `logfwd-core`, I/O
integration in `logfwd-io`. The `CheckpointTracker` is `no_std` compatible
(no alloc needed -- just 4 u64 fields).

### Harness 1: Invariants hold for all action sequences (depth 6)

```rust
#[cfg(kani)]
mod verification {
    use super::*;

    /// Generate a symbolic action. Constrains n_bytes and
    /// last_newline_pos to valid ranges.
    fn symbolic_action() -> Action {
        let tag: u8 = kani::any_where(|&t: &u8| t < 4);
        match tag {
            0 => {
                let n_bytes: u64 = kani::any_where(|&n: &u64| n > 0 && n <= 4096);
                let has_newline: bool = kani::any();
                let last_newline_pos = if has_newline {
                    // Must be within the chunk (0..n_bytes)
                    Some(kani::any_where(|&p: &u64| p < n_bytes))
                } else {
                    None
                };
                Action::Read { n_bytes, last_newline_pos }
            }
            1 => Action::Checkpoint,
            2 => Action::Crash,
            _ => Action::Restart,
        }
    }

    /// Core invariant checker -- called after every action.
    fn check_invariants(t: &CheckpointTracker) {
        // Invariant 1: offset ordering
        assert!(
            t.checkpoint_offset <= t.processed_offset,
            "checkpoint ahead of processed"
        );
        assert!(
            t.processed_offset <= t.read_offset,
            "processed ahead of read"
        );

        // Invariant 2: remainder consistency
        assert!(
            t.processed_offset + t.remainder_len == t.read_offset,
            "remainder inconsistent with offsets"
        );
    }

    /// All invariants hold for any 6-step action sequence.
    #[kani::proof]
    #[kani::unwind(7)]
    #[kani::solver(kissat)]
    fn verify_invariants_6_steps() {
        let resume: u64 = kani::any_where(|&r: &u64| r <= 1_000_000);
        let mut tracker = CheckpointTracker::new(resume);
        check_invariants(&tracker);

        let mut i = 0u32;
        while i < 6 {
            let action = symbolic_action();
            match action {
                Action::Read { n_bytes, last_newline_pos } => {
                    tracker.apply_read(n_bytes, last_newline_pos);
                }
                Action::Checkpoint => {
                    tracker.apply_checkpoint();
                }
                Action::Crash | Action::Restart => {
                    // Crash loses in-memory state; Restart recovers.
                    // Model both as restart (crash without restart is
                    // just "stop" -- no invariant to check).
                    tracker.apply_restart();
                }
            }
            check_invariants(&tracker);
            i += 1;
        }

        // Vacuity guards
        kani::cover!(tracker.remainder_len > 0, "has remainder");
        kani::cover!(
            tracker.checkpoint_offset < tracker.processed_offset,
            "checkpoint behind processed"
        );
        kani::cover!(
            tracker.checkpoint_offset == tracker.processed_offset,
            "checkpoint caught up"
        );
    }
```

### Harness 2: Checkpoint monotonicity

```rust
    /// Checkpoint offset never decreases across any action sequence.
    #[kani::proof]
    #[kani::unwind(7)]
    #[kani::solver(kissat)]
    fn verify_checkpoint_monotonic() {
        let resume: u64 = kani::any_where(|&r: &u64| r <= 1_000_000);
        let mut tracker = CheckpointTracker::new(resume);
        let mut prev_checkpoint = tracker.checkpoint_offset;

        let mut i = 0u32;
        while i < 6 {
            let action = symbolic_action();
            match action {
                Action::Read { n_bytes, last_newline_pos } => {
                    tracker.apply_read(n_bytes, last_newline_pos);
                }
                Action::Checkpoint => {
                    tracker.apply_checkpoint();
                }
                Action::Crash | Action::Restart => {
                    tracker.apply_restart();
                }
            }
            assert!(
                tracker.checkpoint_offset >= prev_checkpoint,
                "checkpoint regressed"
            );
            prev_checkpoint = tracker.checkpoint_offset;
            i += 1;
        }
    }
```

### Harness 3: No data loss -- restart resumes at checkpoint

```rust
    /// After crash + restart, read_offset == checkpoint_offset.
    /// This means no bytes between checkpoint and old processed_offset
    /// are skipped.
    #[kani::proof]
    #[kani::unwind(9)]
    #[kani::solver(kissat)]
    fn verify_no_data_loss_on_crash() {
        let resume: u64 = kani::any_where(|&r: &u64| r <= 1_000_000);
        let mut tracker = CheckpointTracker::new(resume);

        // Do some reads and checkpoints
        let mut i = 0u32;
        while i < 4 {
            let n_bytes: u64 = kani::any_where(|&n: &u64| n > 0 && n <= 4096);
            let has_newline: bool = kani::any();
            let last_newline_pos = if has_newline {
                Some(kani::any_where(|&p: &u64| p < n_bytes))
            } else {
                None
            };
            tracker.apply_read(n_bytes, last_newline_pos);

            if kani::any() {
                tracker.apply_checkpoint();
            }
            i += 1;
        }

        // Record pre-crash state
        let pre_crash_checkpoint = tracker.checkpoint_offset;

        // Crash and restart
        tracker.apply_restart();

        // After restart: we resume exactly at the checkpoint
        assert_eq!(tracker.read_offset, pre_crash_checkpoint);
        assert_eq!(tracker.processed_offset, pre_crash_checkpoint);
        assert_eq!(tracker.remainder_len, 0);

        // The data between checkpoint and old read_offset will be re-read.
        // No data between checkpoint and old processed_offset is skipped.
    }
```

### Harness 4: Processed offset is always at a newline boundary

This property is more subtle. We can't directly verify "processed_offset
is at a newline" because we don't have the byte buffer -- but we can verify
that processed_offset only advances when `last_newline_pos` is `Some`.

```rust
    /// processed_offset only advances via apply_read with a newline.
    /// It never advances on Read(no newline), Checkpoint, or Restart.
    #[kani::proof]
    #[kani::unwind(7)]
    #[kani::solver(kissat)]
    fn verify_processed_advances_only_on_newline() {
        let resume: u64 = kani::any_where(|&r: &u64| r <= 1_000_000);
        let mut tracker = CheckpointTracker::new(resume);

        let mut i = 0u32;
        while i < 6 {
            let prev_processed = tracker.processed_offset;

            let action = symbolic_action();
            match action {
                Action::Read { n_bytes, last_newline_pos } => {
                    tracker.apply_read(n_bytes, last_newline_pos);
                    match last_newline_pos {
                        Some(_) => {
                            // processed_offset should have advanced
                            assert!(
                                tracker.processed_offset >= prev_processed,
                                "processed regressed on read with newline"
                            );
                        }
                        None => {
                            // processed_offset must NOT change
                            assert_eq!(
                                tracker.processed_offset, prev_processed,
                                "processed changed on read without newline"
                            );
                        }
                    }
                }
                Action::Checkpoint => {
                    tracker.apply_checkpoint();
                    assert_eq!(
                        tracker.processed_offset, prev_processed,
                        "processed changed on checkpoint"
                    );
                }
                Action::Crash | Action::Restart => {
                    tracker.apply_restart();
                    // processed resets to checkpoint -- may be less than prev
                }
            }
            i += 1;
        }
    }
```

### Harness 5: Overflow safety

```rust
    /// No arithmetic overflow for realistic offset values.
    /// Kani automatically checks for overflow on every operation.
    #[kani::proof]
    #[kani::unwind(7)]
    #[kani::solver(kissat)]
    fn verify_no_overflow() {
        // Start at a high offset to stress near-u64::MAX behavior.
        // Bound n_bytes to prevent trivial overflow.
        let resume: u64 = kani::any();
        let mut tracker = CheckpointTracker::new(resume);

        let mut i = 0u32;
        while i < 4 {
            let action = symbolic_action();
            match action {
                Action::Read { n_bytes, last_newline_pos } => {
                    // Guard: skip reads that would overflow read_offset.
                    if tracker.read_offset.checked_add(n_bytes).is_some() {
                        tracker.apply_read(n_bytes, last_newline_pos);
                    }
                }
                Action::Checkpoint => tracker.apply_checkpoint(),
                Action::Crash | Action::Restart => tracker.apply_restart(),
            }
            i += 1;
        }
        // Kani automatically asserts no overflow on +, -, etc.
    }
}
```

## How this connects to existing pipeline proofs

The existing `PipelineMachine` Kani proofs in `lifecycle.rs` verify ordered
ACK: "checkpoint advances only when all prior batches are acked." Those
proofs treat the checkpoint value as an opaque `u64`.

The `CheckpointTracker` proofs verify the other half: "the checkpoint value
passed to PipelineMachine is correct (at a newline boundary, accounting for
remainder)."

Together, they form a compositional argument:

1. `CheckpointTracker` proves: the `u64` offset passed to `create_batch`
   is always at a newline boundary and never ahead of actually-processed data.
2. `PipelineMachine` proves: the `u64` offset persisted to the checkpoint
   store is the one from the highest contiguously-acked batch.
3. Therefore: the persisted checkpoint is always at a newline boundary,
   and restart from that checkpoint re-reads from a valid position.

In the future, Kani function contracts (`#[kani::ensures]`) could formalize
this boundary: `CheckpointTracker::checkpointable_offset` has a contract
that guarantees the returned value satisfies the preconditions expected
by `PipelineMachine::create_batch`. This is the `stub_verified` pattern
from s2n-quic and Firecracker.

## Integration with FramedInput

The `CheckpointTracker` replaces the implicit offset arithmetic currently
scattered across `FramedInput` and `TailedFile`. The integration point:

```rust
// In logfwd-io, FramedInput::poll():
// After framing, update the tracker with framing results.
let frame_result = self.framer.frame(&chunk);
self.tracker.apply_read(
    chunk.len() as u64,
    if frame_result.remainder_offset > 0 && frame_result.remainder_offset < chunk.len() {
        // last_newline_pos is one before remainder_offset
        // (remainder_offset points to the byte after the last \n)
        Some((frame_result.remainder_offset - 1) as u64)
    } else if frame_result.remainder_offset == chunk.len() {
        // All bytes consumed (last byte was \n or chunk is empty)
        Some((chunk.len() - 1) as u64)
    } else {
        // No newline found
        None
    },
);

// When the pipeline requests checkpoint data:
fn checkpoint_data(&self) -> Vec<(SourceId, ByteOffset)> {
    vec![(self.source_id, ByteOffset(self.tracker.checkpointable_offset()))]
}
```

## Implementation plan

### Phase 1: Extract pure state machine (1 PR)

1. Create `crates/logfwd-core/src/checkpoint_tracker.rs` with
   `CheckpointTracker`, `Action` enum, and all transition methods.
2. Add `#[cfg(kani)] mod verification` with harnesses 1--5.
3. Add `pub mod checkpoint_tracker;` to `logfwd-core/src/lib.rs`.
4. Run `cargo kani --package logfwd-core` to verify all proofs pass.
5. Add unit tests mirroring the Kani harnesses (same pattern as
   `pipeline/batch.rs`).

### Phase 2: Wire into FramedInput (separate PR)

1. Add `CheckpointTracker` field to `FramedInput`.
2. Update `poll()` to call `apply_read` after framing.
3. Update `checkpoint_data()` to use `checkpointable_offset()`.
4. Remove ad-hoc offset arithmetic from `TailedFile` / `FramedInput`.

### Phase 3: Bolero dual harnesses (optional, stretch)

1. Add `bolero` dependency to `logfwd-core` dev-dependencies.
2. Convert Kani harnesses to the unified Bolero pattern
   (`#[cfg_attr(kani, kani::proof)]` + `bolero::check!()`).
3. Run the same properties under libfuzzer for extended coverage
   beyond Kani's bounded depth.

## Prior art

### s2n-quic (AWS)

s2n-quic runs 30+ Kani/Bolero harnesses in CI. Their pattern for
offset-tracking verification:
- Packet number encode/decode roundtrip: verified across all inputs
  in 2.87 seconds, caught a real bug during optimization.
- RTT weighted average: verified across all u32 x u32 combinations
  in 44.77 seconds.
- Uses `InlineVec<Op, 8>` with `#[kani::unwind(9)]` for operation
  sequences -- confirming ~8 steps as the practical bound.
- Mixes Kissat, CaDiCaL, and MiniSat per-harness based on
  performance profiling.

### Firecracker (AWS)

- Verified virtio descriptor chain parser for all possible guest
  memory contents.
- Used function contracts + `stub_verified` for compositional proof
  of `gcd` -> `TokenBucket::new`.

### Kani function contracts

Since Kani 0.33.0, function contracts (`#[kani::requires]`,
`#[kani::ensures]`, `#[kani::modifies]`) enable compositional
verification via `#[kani::proof_for_contract]` and
`#[kani::stub_verified]`. This is the path to connecting
`CheckpointTracker` proofs with `PipelineMachine` proofs without
a monolithic harness.

## Risks and mitigations

| Risk | Mitigation |
|------|-----------|
| BTreeMap in `apply_ack` bloats SAT formula | CheckpointTracker has no collections -- pure u64 arithmetic. Keep proofs separate from PipelineMachine proofs. |
| u64 overflow near MAX | Harness 5 explicitly tests with unconstrained u64 start offset. Kani auto-checks overflow. Use `checked_add` in production code. |
| Solver timeout at depth 8 | Start at depth 6, profile with `--solver kissat`. Fall back to `cadical` if needed. |
| Remainder prepend logic in FramedInput is complex | The tracker doesn't model buffer content -- only offsets. The "remainder content is correct" property is covered by the existing framer Kani proofs. The tracker covers "remainder offset accounting is correct." |
| Crash can happen at any point (not just between actions) | For checkpoint safety, only the checkpoint offset matters. Mid-action crashes are equivalent to "crash before the action" because none of the in-memory state survives. The atomic write in FileCheckpointStore ensures partial flushes don't corrupt the checkpoint file. |

## References

- `crates/logfwd-core/src/framer.rs` -- existing Kani proofs for newline framing
- `crates/logfwd-core/src/pipeline/` -- existing Kani proofs for ordered ACK
- `crates/logfwd-io/src/checkpoint.rs` -- FileCheckpointStore (I/O layer)
- `crates/logfwd-io/src/framed.rs` -- FramedInput remainder management
- `crates/logfwd-io/src/tail.rs` -- TailedFile offset tracking
- `dev-docs/research/KANI_LIMITS.md` -- Kani solver limits
- `dev-docs/research/offset-checkpoint-research.md` -- checkpoint design research
- [How s2n-quic uses Kani](https://model-checking.github.io//kani-verifier-blog/2023/05/30/how-s2n-quic-uses-kani-to-inspire-confidence.html)
- [Kani function contracts](https://model-checking.github.io/kani-verifier-blog/2024/01/29/function-contracts.html)
- [Kani attributes reference](https://model-checking.github.io/kani/reference/attributes.html)
