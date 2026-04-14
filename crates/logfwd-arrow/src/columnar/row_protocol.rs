// row_protocol.rs — Row lifecycle state machine for columnar builders.
//
// Extracted from StreamingBuilder to isolate the batch/row call-sequence
// protocol.  This is the first scaffold toward a shared ColumnarBatchBuilder
// (see dev-docs/research/columnar-batch-builder.md).
//
// The protocol enforces:
//   Idle → begin_batch() → InBatch
//     InBatch → begin_row() → InRow
//       InRow → end_row() → InBatch
//     InBatch → finish_batch() → Idle

use logfwd_core::scanner::BuilderState;

/// Row lifecycle state machine for columnar builders.
///
/// Tracks the current batch/row phase, row count, per-row dedup bitmask,
/// and the per-row line-written flag.  These four fields are the minimal
/// state needed to enforce the call sequence contract documented on
/// [`BuilderState`] and [`ScanBuilder`](logfwd_core::scanner::ScanBuilder).
///
/// # Why runtime state instead of typestate
///
/// The batch/row protocol is driven by the scanner loop which dispatches
/// `begin_row`/`end_row` dynamically based on input data.  The transitions
/// cannot be encoded at compile time because the number of rows per batch
/// is data-dependent and the same builder instance is reused across
/// batches.  `debug_assert` guards catch illegal transitions in tests
/// while keeping zero overhead in release builds.
///
/// Transition methods are `#[inline(always)]` because they sit on the
/// scanner hot path and must not introduce call overhead.
pub(crate) struct RowLifecycle {
    /// Current phase of the batch build cycle.
    state: BuilderState,
    /// Number of rows completed (via `end_row`) in the current batch.
    row_count: u32,
    /// Per-row dedup bitmask for fields 0–63.  Reset in `begin_row`.
    written_bits: u64,
    /// Whether `append_line` has been called for the current row.
    line_written_this_row: bool,
}

impl RowLifecycle {
    /// Create a new lifecycle in the `Idle` state.
    pub(crate) fn new() -> Self {
        RowLifecycle {
            state: BuilderState::Idle,
            row_count: 0,
            written_bits: 0,
            line_written_this_row: false,
        }
    }

    // -----------------------------------------------------------------
    // State transitions
    // -----------------------------------------------------------------

    /// Transition to `InBatch`.  Resets row count and dedup state.
    ///
    /// # Panics (debug)
    /// Asserts that we are not currently inside a row.
    #[inline(always)]
    pub(crate) fn begin_batch(&mut self) {
        debug_assert!(
            matches!(self.state, BuilderState::Idle | BuilderState::InBatch),
            "begin_batch called while inside a row (missing end_row)"
        );
        self.state = BuilderState::InBatch;
        self.row_count = 0;
        self.written_bits = 0;
    }

    /// Transition from `InBatch` to `InRow`.  Resets per-row dedup state.
    ///
    /// # Panics (debug)
    /// Asserts that we are in the `InBatch` state.
    #[inline(always)]
    pub(crate) fn begin_row(&mut self) {
        debug_assert_eq!(
            self.state,
            BuilderState::InBatch,
            "begin_row called outside of a batch (call begin_batch first)"
        );
        self.written_bits = 0;
        self.line_written_this_row = false;
        self.state = BuilderState::InRow;
    }

    /// Transition from `InRow` back to `InBatch`.  Increments row count.
    ///
    /// # Panics (debug)
    /// Asserts that we are in the `InRow` state.
    ///
    /// # Panics (always)
    /// Panics if row count would overflow `u32::MAX`.
    #[inline(always)]
    pub(crate) fn end_row(&mut self) {
        debug_assert_eq!(
            self.state,
            BuilderState::InRow,
            "end_row called without a matching begin_row"
        );
        self.row_count = self
            .row_count
            .checked_add(1)
            .expect("row_count overflow: batch exceeds u32::MAX rows");
        self.state = BuilderState::InBatch;
    }

    /// Transition from `InBatch` to `Idle` after batch finalization.
    ///
    /// # Panics (debug)
    /// Asserts that we are in the `InBatch` state.
    #[inline(always)]
    pub(crate) fn finish_batch(&mut self) {
        debug_assert_eq!(
            self.state,
            BuilderState::InBatch,
            "finish_batch called outside of a batch"
        );
        self.state = BuilderState::Idle;
    }

    // -----------------------------------------------------------------
    // Accessors — all #[inline(always)] for the hot path
    // -----------------------------------------------------------------

    /// Current protocol state.
    #[inline(always)]
    pub(crate) fn state(&self) -> BuilderState {
        self.state
    }

    /// Number of completed rows in the current batch.
    #[inline(always)]
    pub(crate) fn row_count(&self) -> u32 {
        self.row_count
    }

    /// Mutable reference to the per-row dedup bitmask.
    #[inline(always)]
    pub(crate) fn written_bits_mut(&mut self) -> &mut u64 {
        &mut self.written_bits
    }

    /// Whether `append_line` has already been called for the current row.
    #[inline(always)]
    pub(crate) fn line_written_this_row(&self) -> bool {
        self.line_written_this_row
    }

    /// Mark `append_line` as called for the current row.
    #[inline(always)]
    pub(crate) fn set_line_written(&mut self) {
        self.line_written_this_row = true;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_idle() {
        let lc = RowLifecycle::new();
        assert_eq!(lc.state(), BuilderState::Idle);
        assert_eq!(lc.row_count(), 0);
    }

    #[test]
    fn normal_lifecycle_transitions() {
        let mut lc = RowLifecycle::new();

        lc.begin_batch();
        assert_eq!(lc.state(), BuilderState::InBatch);
        assert_eq!(lc.row_count(), 0);

        lc.begin_row();
        assert_eq!(lc.state(), BuilderState::InRow);
        assert!(!lc.line_written_this_row());

        lc.end_row();
        assert_eq!(lc.state(), BuilderState::InBatch);
        assert_eq!(lc.row_count(), 1);

        lc.begin_row();
        lc.end_row();
        assert_eq!(lc.row_count(), 2);

        lc.finish_batch();
        assert_eq!(lc.state(), BuilderState::Idle);
    }

    #[test]
    fn begin_row_resets_dedup_state() {
        let mut lc = RowLifecycle::new();
        lc.begin_batch();

        // Simulate writing fields in the first row.
        lc.begin_row();
        *lc.written_bits_mut() = 0xFF;
        lc.set_line_written();
        assert!(lc.line_written_this_row());
        lc.end_row();

        // Second row starts fresh.
        lc.begin_row();
        assert_eq!(*lc.written_bits_mut(), 0);
        assert!(!lc.line_written_this_row());
        lc.end_row();
        lc.finish_batch();
    }

    #[test]
    fn begin_batch_resets_row_count() {
        let mut lc = RowLifecycle::new();

        lc.begin_batch();
        lc.begin_row();
        lc.end_row();
        lc.begin_row();
        lc.end_row();
        assert_eq!(lc.row_count(), 2);
        lc.finish_batch();

        // Second batch starts fresh.
        lc.begin_batch();
        assert_eq!(lc.row_count(), 0);
        lc.finish_batch();
    }

    #[test]
    fn empty_batch_allowed() {
        let mut lc = RowLifecycle::new();
        lc.begin_batch();
        assert_eq!(lc.row_count(), 0);
        lc.finish_batch();
        assert_eq!(lc.state(), BuilderState::Idle);
    }

    #[test]
    fn begin_batch_from_idle_and_in_batch_both_work() {
        let mut lc = RowLifecycle::new();
        // From Idle.
        lc.begin_batch();
        assert_eq!(lc.state(), BuilderState::InBatch);
        // From InBatch (restart without finishing — allowed by the protocol).
        lc.begin_batch();
        assert_eq!(lc.state(), BuilderState::InBatch);
        lc.finish_batch();
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "begin_row called outside of a batch")]
    fn begin_row_without_batch_panics() {
        let mut lc = RowLifecycle::new();
        lc.begin_row();
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "end_row called without a matching begin_row")]
    fn end_row_without_begin_row_panics() {
        let mut lc = RowLifecycle::new();
        lc.begin_batch();
        lc.end_row();
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "begin_batch called while inside a row")]
    fn begin_batch_inside_row_panics() {
        let mut lc = RowLifecycle::new();
        lc.begin_batch();
        lc.begin_row();
        lc.begin_batch();
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "finish_batch called outside of a batch")]
    fn finish_batch_from_idle_panics() {
        let mut lc = RowLifecycle::new();
        lc.finish_batch();
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "finish_batch called outside of a batch")]
    fn finish_batch_inside_row_panics() {
        let mut lc = RowLifecycle::new();
        lc.begin_batch();
        lc.begin_row();
        lc.finish_batch();
    }
}

// ---------------------------------------------------------------------------
// Kani proofs — exhaustive state machine verification
// ---------------------------------------------------------------------------
//
// Pattern: symbolic action sequences (same technique as checkpoint_tracker.rs).
// Explores all valid transition orderings to prove safety invariants hold
// for every reachable state.
#[cfg(kani)]
mod verification {
    use super::*;

    /// Dispatch a valid action based on current state.
    /// Returns the action tag for coverage tracking.
    fn symbolic_action(lc: &mut RowLifecycle) -> u8 {
        let tag: u8 = kani::any();
        match lc.state() {
            // Idle: only begin_batch is valid.
            BuilderState::Idle => {
                kani::assume(tag == 0);
                lc.begin_batch();
            }
            // InBatch: begin_row (1), finish_batch (2), or begin_batch restart (0).
            BuilderState::InBatch => {
                kani::assume(tag <= 2);
                match tag {
                    0 => lc.begin_batch(),
                    1 => lc.begin_row(),
                    _ => lc.finish_batch(),
                }
            }
            // InRow: end_row is the only valid transition.
            BuilderState::InRow => {
                kani::assume(tag == 3);
                lc.end_row();
            }
            _ => unreachable!(),
        }
        tag
    }

    /// Row count never decreases within a batch and resets on begin_batch.
    ///
    /// 8-step symbolic action sequence covers batches with 0–3 rows plus
    /// cross-batch restart (begin_batch + 3×(begin_row+end_row) + finish = 8).
    #[kani::proof]
    #[kani::solver(kissat)]
    #[kani::unwind(10)]
    fn verify_row_count_monotonic() {
        let mut lc = RowLifecycle::new();
        let mut prev_count: u32 = 0;

        for _ in 0..8 {
            let tag = symbolic_action(&mut lc);
            let count = lc.row_count();

            if tag == 0 {
                // begin_batch resets to 0.
                assert_eq!(count, 0, "begin_batch must reset row_count");
                prev_count = 0;
            } else if tag == 3 {
                // end_row increments by exactly 1.
                assert_eq!(count, prev_count + 1, "end_row must increment by 1");
                prev_count = count;
            } else {
                // Other actions preserve row_count.
                assert_eq!(count, prev_count, "non-row actions preserve count");
            }
        }

        kani::cover!(prev_count > 0, "at least one row completed");
        kani::cover!(prev_count > 2, "multiple rows completed");
    }

    /// written_bits is zero at the start of every row.
    #[kani::proof]
    #[kani::solver(kissat)]
    #[kani::unwind(7)]
    fn verify_written_bits_reset_on_begin_row() {
        let mut lc = RowLifecycle::new();

        for _ in 0..5 {
            let tag = symbolic_action(&mut lc);

            if tag == 1 {
                // begin_row just fired → bits must be zero.
                assert_eq!(
                    *lc.written_bits_mut(),
                    0,
                    "written_bits must be zero after begin_row"
                );
                // Simulate field writes in this row.
                *lc.written_bits_mut() = kani::any();
            }
        }

        kani::cover!(lc.row_count() > 0, "completed at least one row");
    }

    /// State machine only reaches valid states: Idle, InBatch, InRow.
    #[kani::proof]
    #[kani::solver(kissat)]
    #[kani::unwind(9)]
    fn verify_state_always_valid() {
        let mut lc = RowLifecycle::new();

        for _ in 0..7 {
            symbolic_action(&mut lc);
            let s = lc.state();
            assert!(
                matches!(
                    s,
                    BuilderState::Idle | BuilderState::InBatch | BuilderState::InRow
                ),
                "state must always be a valid RowLifecycle state"
            );
        }

        kani::cover!(lc.state() == BuilderState::Idle, "reached Idle");
        kani::cover!(lc.state() == BuilderState::InBatch, "reached InBatch");
        kani::cover!(lc.state() == BuilderState::InRow, "reached InRow");
    }
}
