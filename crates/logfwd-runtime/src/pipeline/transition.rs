use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// High-level runtime area that emitted a transition event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionPhase {
    /// Batch ownership or lifecycle changed.
    Batch,
    /// Output worker or worker-pool delivery outcome.
    WorkerPool,
    /// A checkpoint ticket disposition was applied.
    Ticket,
    /// Checkpoint state or durable flush activity changed.
    Checkpoint,
    /// Pipeline lifecycle state changed.
    Lifecycle,
}

/// Specific transition action within a [`TransitionPhase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionAction {
    /// A batch ticket entered the Sending state.
    EnterSending,
    /// A sink or worker-pool delivery outcome was produced.
    SinkOutcome,
    /// A ticket disposition was applied by the pipeline.
    ApplyDisposition,
    /// A checkpoint value advanced or was persisted.
    UpdateCheckpoint,
    /// A checkpoint store flush attempt completed.
    FlushCheckpoint,
    /// The pipeline began graceful drain.
    BeginDrain,
    /// The pipeline attempted to stop after drain.
    Stop,
    /// The pipeline force-stopped unresolved in-flight work.
    ForceStop,
}

/// Outcome attached to a transition event when the action has a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionOutcome {
    /// The transition was entered.
    Entered,
    /// The batch reached the sink successfully.
    Delivered,
    /// The sink rejected the batch permanently.
    Rejected,
    /// Retries were exhausted before delivery.
    RetryExhausted,
    /// A hard timeout elapsed.
    TimedOut,
    /// The worker pool was already closed.
    PoolClosed,
    /// The selected worker channel was closed.
    WorkerChannelClosed,
    /// No worker could accept the batch.
    NoWorkersAvailable,
    /// The runtime hit an internal failure path.
    InternalFailure,
    /// The ticket was acknowledged.
    Acked,
    /// The ticket was rejected.
    RejectedTicket,
    /// The ticket was held unresolved.
    Held,
    /// The checkpoint advanced.
    Advanced,
    /// The action completed without advancing checkpoint state.
    Unchanged,
    /// The flush attempt succeeded.
    FlushSucceeded,
    /// The flush attempt failed.
    FlushFailed,
    /// The lifecycle transition started.
    Started,
    /// The lifecycle transition completed.
    Completed,
    /// The graceful stop was blocked by unresolved work.
    Blocked,
    /// The pipeline was forced to stop.
    Forced,
}

/// Disposition selected for a Sending ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionDisposition {
    /// Commit this ticket as delivered.
    Ack,
    /// Commit this ticket as permanently rejected.
    Reject,
    /// Leave this ticket unresolved so checkpoint state cannot advance.
    Hold,
}

/// Structured runtime transition event emitted by the real pipeline paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionEvent {
    /// Monotonic sequence assigned by the installed emitter handle.
    pub seq: u64,
    /// Wall-clock timestamp in nanoseconds since the Unix epoch.
    pub timestamp_nanos: u64,
    /// Runtime batch id from `PipelineMetrics`, when available.
    pub batch_id: Option<u64>,
    /// Typestate ticket id from `PipelineMachine`, when available.
    pub ticket_id: Option<u64>,
    /// Source id associated with the ticket/checkpoint, when available.
    pub source_id: Option<u64>,
    /// High-level runtime area that emitted the event.
    pub phase: TransitionPhase,
    /// Specific transition action.
    pub action: TransitionAction,
    /// Result of the action, when available.
    pub outcome: Option<TransitionOutcome>,
    /// Ticket disposition, when the event applies one.
    pub disposition: Option<TransitionDisposition>,
    /// Checkpoint byte offset, when the event is source/checkpoint-scoped.
    pub checkpoint_offset: Option<u64>,
    /// Worker id that produced a sink outcome, when applicable.
    pub worker_id: Option<usize>,
    /// One-based flush attempt number, when applicable.
    pub attempt: Option<u32>,
    /// Number of unresolved in-flight batches for lifecycle events.
    pub in_flight_count: Option<usize>,
}

impl TransitionEvent {
    /// Create an event with common identity populated by the emitter handle.
    pub const fn new(
        seq: u64,
        timestamp_nanos: u64,
        phase: TransitionPhase,
        action: TransitionAction,
    ) -> Self {
        Self {
            seq,
            timestamp_nanos,
            batch_id: None,
            ticket_id: None,
            source_id: None,
            phase,
            action,
            outcome: None,
            disposition: None,
            checkpoint_offset: None,
            worker_id: None,
            attempt: None,
            in_flight_count: None,
        }
    }
}

/// Consumer for runtime transition events.
pub trait TransitionEventEmitter: Send + Sync + 'static {
    /// Emit one transition event.
    fn emit(&self, event: TransitionEvent);
}

/// Explicit no-op emitter for callers that want a concrete default sink.
#[derive(Debug, Default)]
pub struct NoopTransitionEventEmitter;

impl TransitionEventEmitter for NoopTransitionEventEmitter {
    fn emit(&self, _event: TransitionEvent) {}
}

/// In-memory recorder useful for tests that need to inspect emitted events.
#[derive(Debug, Default)]
pub struct TransitionEventRecorder {
    events: Mutex<Vec<TransitionEvent>>,
}

impl TransitionEventRecorder {
    /// Return a cloned snapshot of all events recorded so far.
    pub fn snapshot(&self) -> Vec<TransitionEvent> {
        self.events
            .lock()
            .expect("transition event recorder mutex poisoned during snapshot")
            .clone()
    }

    /// Drain all recorded events.
    pub fn drain(&self) -> Vec<TransitionEvent> {
        let mut events = self
            .events
            .lock()
            .expect("transition event recorder mutex poisoned during drain");
        std::mem::take(&mut *events)
    }
}

impl TransitionEventEmitter for TransitionEventRecorder {
    fn emit(&self, event: TransitionEvent) {
        self.events
            .lock()
            .expect("transition event recorder mutex poisoned during emit")
            .push(event);
    }
}

impl TransitionEventEmitter for tokio::sync::mpsc::UnboundedSender<TransitionEvent> {
    fn emit(&self, event: TransitionEvent) {
        let _ = self.send(event);
    }
}

#[derive(Clone, Default)]
pub(crate) struct TransitionEventEmitterHandle {
    inner: Option<Arc<dyn TransitionEventEmitter>>,
    seq: Arc<AtomicU64>,
}

impl TransitionEventEmitterHandle {
    pub(crate) fn new(emitter: Arc<dyn TransitionEventEmitter>) -> Self {
        Self {
            inner: Some(emitter),
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn noop() -> Self {
        Self::default()
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub(crate) fn emit_with<F>(&self, build: F)
    where
        F: FnOnce(u64, u64) -> TransitionEvent,
    {
        if let Some(emitter) = &self.inner {
            let seq = self.seq.fetch_add(1, Ordering::Relaxed);
            emitter.emit(build(seq, now_nanos()));
        }
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}
