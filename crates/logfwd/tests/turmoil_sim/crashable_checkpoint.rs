//! Checkpoint store with crash simulation for testing at-least-once guarantees.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use logfwd_io::checkpoint::{CheckpointStore, SourceCheckpoint};

/// A `CheckpointStore` that can simulate crashes between write and fsync.
///
/// Maintains two tiers of state:
/// - **durable**: survives crashes (like data already fsync'd to disk)
/// - **pending**: lost on crash (like data written but not fsync'd)
///
/// When `crash_armed` is set, the next `flush()` call "crashes" — pending
/// data is discarded and an error is returned.
pub struct CrashableCheckpointStore {
    durable: BTreeMap<u64, SourceCheckpoint>,
    pending: BTreeMap<u64, SourceCheckpoint>,
    crash_armed: Arc<AtomicBool>,
    flush_count: u64,
}

impl CrashableCheckpointStore {
    pub fn new() -> Self {
        Self {
            durable: BTreeMap::new(),
            pending: BTreeMap::new(),
            crash_armed: Arc::new(AtomicBool::new(false)),
            flush_count: 0,
        }
    }

    /// Get a handle to arm crashes from outside (e.g., from a test closure).
    pub fn crash_handle(&self) -> Arc<AtomicBool> {
        self.crash_armed.clone()
    }

    /// Get the durable checkpoint state (what survives a crash).
    #[allow(dead_code)]
    pub fn durable_offset(&self, source_id: u64) -> Option<u64> {
        self.durable.get(&source_id).map(|c| c.offset)
    }

    /// Number of successful flushes.
    #[allow(dead_code)]
    pub fn flush_count(&self) -> u64 {
        self.flush_count
    }
}

impl CheckpointStore for CrashableCheckpointStore {
    fn update(&mut self, checkpoint: SourceCheckpoint) {
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
