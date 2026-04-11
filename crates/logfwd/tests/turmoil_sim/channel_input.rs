//! InputSource backed by pre-loaded data for simulation testing.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use logfwd_io::input::{InputEvent, InputSource};
use logfwd_io::tail::ByteOffset;
use logfwd_types::diagnostics::ComponentHealth;
use logfwd_types::pipeline::SourceId;

use super::trace_bridge::TraceRecorder;

/// A mock InputSource that returns pre-loaded chunks one at a time.
pub struct ChannelInputSource {
    name: String,
    chunks: VecDeque<Vec<u8>>,
    source_id: SourceId,
    offset: u64,
    trace: Option<TraceRecorder>,
    last_traced_checkpoint: AtomicU64,
}

impl ChannelInputSource {
    pub fn new(name: &str, source_id: SourceId, data: Vec<Vec<u8>>) -> Self {
        Self {
            name: name.to_string(),
            chunks: data.into(),
            source_id,
            offset: 0,
            trace: None,
            last_traced_checkpoint: AtomicU64::new(0),
        }
    }

    /// Attach a trace recorder for runtime-originated checkpoint snapshots.
    ///
    /// The runtime calls `checkpoint_data()` while constructing a batch for
    /// send; that is the turmoil harness' observable `begin_send` boundary.
    pub fn with_trace_recorder(mut self, trace: TraceRecorder) -> Self {
        self.trace = Some(trace);
        self
    }
}

impl InputSource for ChannelInputSource {
    fn poll(&mut self) -> io::Result<Vec<InputEvent>> {
        match self.chunks.pop_front() {
            Some(data) => {
                let accounted_bytes = data.len() as u64;
                self.offset += data.len() as u64;
                Ok(vec![InputEvent::Data {
                    bytes: data,
                    source_id: Some(self.source_id),
                    accounted_bytes,
                }])
            }
            None => Ok(vec![]),
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn health(&self) -> ComponentHealth {
        // The turmoil simulation source is preloaded and has no independent
        // runtime lifecycle beyond the harness driving its queued chunks.
        ComponentHealth::Healthy
    }

    fn checkpoint_data(&self) -> Vec<(SourceId, ByteOffset)> {
        if self.offset > 0 {
            if let Some(trace) = &self.trace {
                let previous = self
                    .last_traced_checkpoint
                    .swap(self.offset, Ordering::SeqCst);
                if previous != self.offset {
                    trace.record_batch_begin(self.source_id.0, self.offset);
                }
            }
            vec![(self.source_id, ByteOffset(self.offset))]
        } else {
            vec![]
        }
    }
}
