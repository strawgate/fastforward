//! Instrumented sink with programmable failure scripts for simulation testing.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arrow::record_batch::RecordBatch;
use logfwd_output::BatchMetadata;
use logfwd_output::sink::{SendResult, Sink};

/// What the sink should do on a given call.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum FailureAction {
    /// Succeed immediately.
    Succeed,
    /// Return RetryAfter with the given duration.
    RetryAfter(Duration),
    /// Return an IO error with the given kind.
    IoError(io::ErrorKind),
    /// Reject with the given reason.
    Reject(String),
    /// Succeed after a delay (simulates slow delivery).
    Delay(Duration),
}

/// Sink that follows a script of actions and records all calls.
///
/// Each `send_batch` call consumes the next action from the script.
/// When the script is exhausted, subsequent calls succeed immediately.
/// All calls are counted via shared atomic counters for test assertions.
pub struct InstrumentedSink {
    script: Vec<FailureAction>,
    call_index: usize,
    delivered_rows: Arc<AtomicU64>,
    call_count: Arc<AtomicU64>,
}

#[allow(dead_code)]
impl InstrumentedSink {
    pub fn new(script: Vec<FailureAction>) -> Self {
        Self {
            script,
            call_index: 0,
            delivered_rows: Arc::new(AtomicU64::new(0)),
            call_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Create a sink that always succeeds.
    pub fn always_succeed() -> Self {
        Self::new(vec![])
    }

    /// Get shared counter for delivered rows.
    pub fn delivered_counter(&self) -> Arc<AtomicU64> {
        self.delivered_rows.clone()
    }

    /// Get shared counter for total calls.
    pub fn call_counter(&self) -> Arc<AtomicU64> {
        self.call_count.clone()
    }

    fn next_action(&mut self) -> FailureAction {
        let idx = self.call_index;
        self.call_index += 1;
        if idx < self.script.len() {
            self.script[idx].clone()
        } else {
            FailureAction::Succeed
        }
    }
}

impl Sink for InstrumentedSink {
    fn send_batch<'a>(
        &'a mut self,
        batch: &'a RecordBatch,
        _metadata: &'a BatchMetadata,
    ) -> Pin<Box<dyn Future<Output = io::Result<SendResult>> + Send + 'a>> {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        let action = self.next_action();
        let rows = batch.num_rows() as u64;
        let delivered = self.delivered_rows.clone();

        Box::pin(async move {
            match action {
                FailureAction::Succeed => {
                    delivered.fetch_add(rows, Ordering::Relaxed);
                    Ok(SendResult::Ok)
                }
                FailureAction::RetryAfter(dur) => Ok(SendResult::RetryAfter(dur)),
                FailureAction::IoError(kind) => Err(io::Error::new(kind, "simulated failure")),
                FailureAction::Reject(reason) => Ok(SendResult::Rejected(reason)),
                FailureAction::Delay(dur) => {
                    tokio::time::sleep(dur).await;
                    delivered.fetch_add(rows, Ordering::Relaxed);
                    Ok(SendResult::Ok)
                }
            }
        })
    }

    fn flush(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn name(&self) -> &str {
        "instrumented"
    }

    fn shutdown(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
}
