//! Pure pipeline state machine — formally verified batch lifecycle.
//!
//! Separates pipeline decision logic from IO/async. The Rust compiler
//! enforces state transitions via typestate pattern (illegal transitions
//! are compile errors). Kani proves business invariants (11 proofs).
//! TLA+ liveness modeling is planned in Phase 7.
//!
//! # Architecture
//!
//! Two typestate machines:
//!
//! - [`BatchTicket`]`<S>`: Per-batch lifecycle (Queued → Sending → Acked/Rejected).
//!   `#[must_use]` + self-consuming transitions prevent double-ack and silent drops.
//!   Fields are private; only [`PipelineMachine`] can create tickets.
//!
//! - [`PipelineMachine`]`<S>`: Pipeline lifecycle (Starting → Running → Draining → Stopped)
//!   with ordered ACK offset tracking. Committed offset advances only when ALL
//!   prior batches for a source are acked (Filebeat registrar pattern).

mod batch;
mod lifecycle;

// Batch ticket types
pub use batch::{AckReceipt, BatchId, BatchTicket, Queued, Sending, SourceId};

// Pipeline lifecycle types
pub use lifecycle::{
    CommitAdvance, CreateBatchError, Draining, PipelineMachine, Running, Starting, Stopped,
};
