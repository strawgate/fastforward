//! Async output worker pool with MRU-first work consolidation.
//!
//! # Design
//!
//! Workers are long-lived tokio tasks, each owning one `Sink` instance
//! (and therefore its own HTTP connection pool). Workers are kept in a
//! `std::collections::VecDeque` ordered Most-Recently-Used first. Dispatch always tries the
//! front worker first; only when that channel is full does it try the next,
//! and so on. This **consolidates work onto the fewest active workers**,
//! keeping cold workers idle long enough to hit their `idle_timeout` and
//! self-terminate — which closes their HTTP connections.
//!
//! # Scaling
//!
//! - Under low load: 1 active worker, rest idle → eventually close.
//! - Under burst: pool spawns workers (up to `max_workers`) one at a time.
//! - At `max_workers` with all channels full: `submit` async-waits on the
//!   front worker, providing natural back-pressure to the pipeline.
//!
//! # Safety invariants
//!
//! - Every submitted [`WorkItem`] is either delivered to a worker's channel
//!   or async-waited until a worker has capacity. Items are never dropped.
//! - Every in-flight batch ticket is acked or rejected before shutdown
//!   completes. The `drain` method joins all worker tasks.
//! - Worker panic is surfaced when the pool joins worker tasks during
//!   [`OutputWorkerPool::drain`]. Closed worker channels are pruned lazily on
//!   the next submit, and worker-slot cleanup is drop-guarded so control-plane
//!   health does not retain stale live-worker state after abrupt exits.
//!
//! # Kani proofs
//!
//! Pure dispatch logic is extracted into `dispatch_step` and proved with
//! Kani.

mod dispatch;
mod health;
mod pool;
mod types;
mod worker;

use logfwd_types::pipeline::{BatchTicket, Sending};

use crate::pipeline::transition::TransitionEventEmitterHandle;
use crate::pipeline::{TransitionAction, TransitionEvent, TransitionOutcome, TransitionPhase};

pub use pool::OutputWorkerPool;
pub use types::{AckItem, DeliveryOutcome, WorkItem};

/// Maps a worker pool [`DeliveryOutcome`] to the pipeline-level [`TransitionOutcome`]
/// used by the checkpoint and diagnostics subsystems.
pub(super) const fn transition_outcome_for_delivery(
    outcome: &DeliveryOutcome,
) -> TransitionOutcome {
    match outcome {
        DeliveryOutcome::Delivered => TransitionOutcome::Delivered,
        DeliveryOutcome::Rejected { .. } => TransitionOutcome::Rejected,
        DeliveryOutcome::RetryExhausted => TransitionOutcome::RetryExhausted,
        DeliveryOutcome::TimedOut => TransitionOutcome::TimedOut,
        DeliveryOutcome::PoolClosed => TransitionOutcome::PoolClosed,
        DeliveryOutcome::WorkerChannelClosed => TransitionOutcome::WorkerChannelClosed,
        DeliveryOutcome::NoWorkersAvailable => TransitionOutcome::NoWorkersAvailable,
        DeliveryOutcome::InternalFailure => TransitionOutcome::InternalFailure,
    }
}

/// Emits a [`TransitionEvent`] for each ticket in the batch (or a single
/// event when `tickets` is empty) recording the delivery outcome against the
/// diagnostics transition log.
pub(super) fn emit_delivery_outcome(
    transition_events: &TransitionEventEmitterHandle,
    batch_id: u64,
    tickets: &[BatchTicket<Sending, u64>],
    worker_id: Option<usize>,
    outcome: TransitionOutcome,
) {
    if !transition_events.is_enabled() {
        return;
    }

    if tickets.is_empty() {
        transition_events.emit_with(|seq, timestamp_nanos| {
            let mut event = TransitionEvent::new(
                seq,
                timestamp_nanos,
                TransitionPhase::WorkerPool,
                TransitionAction::SinkOutcome,
            );
            event.batch_id = Some(batch_id);
            event.worker_id = worker_id;
            event.outcome = Some(outcome);
            event
        });
        return;
    }

    for ticket in tickets {
        transition_events.emit_with(|seq, timestamp_nanos| {
            let mut event = TransitionEvent::new(
                seq,
                timestamp_nanos,
                TransitionPhase::WorkerPool,
                TransitionAction::SinkOutcome,
            );
            event.batch_id = Some(batch_id);
            event.ticket_id = Some(ticket.id().0);
            event.source_id = Some(ticket.source().0);
            event.checkpoint_offset = Some(*ticket.checkpoint());
            event.worker_id = worker_id;
            event.outcome = Some(outcome);
            event
        });
    }
}
