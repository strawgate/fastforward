//! Turmoil barrier trigger types for deterministic seam interleaving tests.
//!
//! These events are only compiled when `logfwd-runtime` is built with
//! `feature = "turmoil"`. Production builds are unaffected.

use crate::worker_pool::DeliveryOutcome;

/// Barrier events emitted by runtime seam hooks.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RuntimeBarrierEvent {
    /// Emitted by worker tasks immediately before sending an ack item.
    BeforeWorkerAckSend {
        worker_id: usize,
        batch_id: u64,
        outcome: DeliveryOutcome,
        retries: usize,
    },
    /// Emitted by checkpoint I/O immediately before each flush attempt.
    BeforeCheckpointFlushAttempt { attempt: u32 },
}

/// Trigger a Turmoil barrier event.
pub async fn trigger(event: RuntimeBarrierEvent) {
    turmoil::barriers::trigger(event).await;
}
