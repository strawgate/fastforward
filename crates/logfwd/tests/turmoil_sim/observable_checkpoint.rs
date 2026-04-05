//! Checkpoint store that records full update history for invariant verification.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use logfwd_io::checkpoint::{CheckpointStore, SourceCheckpoint};

/// Entry in the checkpoint update history.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CheckpointEvent {
    pub source_id: u64,
    pub offset: u64,
    pub flush_number: u64,
}

/// A `CheckpointStore` that records full update history and can simulate crashes.
///
/// Maintains two tiers of state:
/// - **durable**: survives crashes (like data already fsync'd to disk)
/// - **pending**: lost on crash (like data written but not fsync'd)
///
/// All `update()` calls are recorded in `update_history` for post-test
/// invariant verification (monotonicity, upper bounds, etc.).
pub struct ObservableCheckpointStore {
    durable: BTreeMap<u64, SourceCheckpoint>,
    pending: BTreeMap<u64, SourceCheckpoint>,
    crash_armed: Arc<AtomicBool>,
    flush_count: u64,
    /// Full history of all update() calls, in order.
    pub update_history: Vec<CheckpointEvent>,
}

#[allow(dead_code)]
impl ObservableCheckpointStore {
    pub fn new() -> Self {
        Self {
            durable: BTreeMap::new(),
            pending: BTreeMap::new(),
            crash_armed: Arc::new(AtomicBool::new(false)),
            flush_count: 0,
            update_history: Vec::new(),
        }
    }

    /// Get a handle to arm crashes from outside (e.g., from a test closure).
    pub fn crash_handle(&self) -> Arc<AtomicBool> {
        self.crash_armed.clone()
    }

    /// Get the durable checkpoint offset for a source (what survives a crash).
    pub fn durable_offset(&self, source_id: u64) -> Option<u64> {
        self.durable.get(&source_id).map(|c| c.offset)
    }

    /// Number of successful flushes.
    pub fn flush_count(&self) -> u64 {
        self.flush_count
    }

    /// Verify that offsets for a given source never decrease in the update history.
    ///
    /// Panics with a descriptive message if a regression is found.
    pub fn assert_monotonic(&self, source_id: u64) {
        let mut last_offset: Option<u64> = None;
        for (i, event) in self.update_history.iter().enumerate() {
            if event.source_id != source_id {
                continue;
            }
            if let Some(prev) = last_offset {
                assert!(
                    event.offset >= prev,
                    "checkpoint regression for source {source_id} at history index {i}: \
                     offset went from {prev} to {}",
                    event.offset
                );
            }
            last_offset = Some(event.offset);
        }
    }

    /// Verify that the durable checkpoint for a source never exceeds a given bound.
    ///
    /// Panics if the durable offset is greater than `max_offset`.
    pub fn assert_checkpoint_le(&self, source_id: u64, max_offset: u64) {
        if let Some(offset) = self.durable_offset(source_id) {
            assert!(
                offset <= max_offset,
                "durable checkpoint for source {source_id} is {offset}, \
                 expected <= {max_offset}"
            );
        }
    }
}

impl CheckpointStore for ObservableCheckpointStore {
    fn update(&mut self, checkpoint: SourceCheckpoint) {
        self.update_history.push(CheckpointEvent {
            source_id: checkpoint.source_id,
            offset: checkpoint.offset,
            flush_number: self.flush_count,
        });
        self.pending.insert(checkpoint.source_id, checkpoint);
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.crash_armed.swap(false, Ordering::SeqCst) {
            // Simulate crash: pending data is lost (not fsync'd)
            self.pending.clear();
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "simulated crash during checkpoint flush",
            ));
        }
        // Normal flush: pending becomes durable
        let pending = std::mem::take(&mut self.pending);
        for (k, v) in pending {
            self.durable.insert(k, v);
        }
        self.flush_count += 1;
        Ok(())
    }

    fn load(&self, source_id: u64) -> Option<SourceCheckpoint> {
        self.durable.get(&source_id).cloned()
    }

    fn load_all(&self) -> Vec<SourceCheckpoint> {
        self.durable.values().cloned().collect()
    }
}
