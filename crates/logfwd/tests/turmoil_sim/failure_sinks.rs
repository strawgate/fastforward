//! Sinks that simulate network failure modes for simulation testing.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arrow::record_batch::RecordBatch;
use logfwd_output::sink::{SendResult, Sink};
use logfwd_output::BatchMetadata;

/// A sink that returns `RetryAfter` for the first N calls, then succeeds.
/// Simulates a server returning HTTP 503/429 before recovering.
pub struct RetryThenSucceedSink {
    fail_remaining: u32,
    retry_delay: Duration,
    delivered: Arc<AtomicU64>,
}

impl RetryThenSucceedSink {
    pub fn new(fail_count: u32, retry_delay: Duration, delivered: Arc<AtomicU64>) -> Self {
        Self {
            fail_remaining: fail_count,
            retry_delay,
            delivered,
        }
    }
}

impl Sink for RetryThenSucceedSink {
    fn send_batch<'a>(
        &'a mut self,
        batch: &'a RecordBatch,
        _metadata: &'a BatchMetadata,
    ) -> Pin<Box<dyn Future<Output = io::Result<SendResult>> + Send + 'a>> {
        if self.fail_remaining > 0 {
            self.fail_remaining -= 1;
            let delay = self.retry_delay;
            Box::pin(async move { Ok(SendResult::RetryAfter(delay)) })
        } else {
            let rows = batch.num_rows() as u64;
            self.delivered.fetch_add(rows, Ordering::Relaxed);
            Box::pin(async { Ok(SendResult::Ok) })
        }
    }

    fn flush(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn name(&self) -> &str {
        "retry-then-succeed"
    }

    fn shutdown(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
}

/// A sink that adds latency to every send_batch call.
/// Simulates a slow network or overloaded server.
pub struct HighLatencySink {
    latency: Duration,
    delivered: Arc<AtomicU64>,
}

impl HighLatencySink {
    pub fn new(latency: Duration, delivered: Arc<AtomicU64>) -> Self {
        Self { latency, delivered }
    }
}

impl Sink for HighLatencySink {
    fn send_batch<'a>(
        &'a mut self,
        batch: &'a RecordBatch,
        _metadata: &'a BatchMetadata,
    ) -> Pin<Box<dyn Future<Output = io::Result<SendResult>> + Send + 'a>> {
        let rows = batch.num_rows() as u64;
        let delivered = self.delivered.clone();
        let latency = self.latency;
        Box::pin(async move {
            tokio::time::sleep(latency).await;
            delivered.fetch_add(rows, Ordering::Relaxed);
            Ok(SendResult::Ok)
        })
    }

    fn flush(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn name(&self) -> &str {
        "high-latency"
    }

    fn shutdown(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
}

/// A sink that returns IO errors for the first N calls (simulating
/// network partition / connection refused), then succeeds.
pub struct PartitionThenRecoverSink {
    fail_remaining: u32,
    delivered: Arc<AtomicU64>,
}

impl PartitionThenRecoverSink {
    pub fn new(fail_count: u32, delivered: Arc<AtomicU64>) -> Self {
        Self {
            fail_remaining: fail_count,
            delivered,
        }
    }
}

impl Sink for PartitionThenRecoverSink {
    fn send_batch<'a>(
        &'a mut self,
        batch: &'a RecordBatch,
        _metadata: &'a BatchMetadata,
    ) -> Pin<Box<dyn Future<Output = io::Result<SendResult>> + Send + 'a>> {
        if self.fail_remaining > 0 {
            self.fail_remaining -= 1;
            Box::pin(async {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "simulated network partition",
                ))
            })
        } else {
            let rows = batch.num_rows() as u64;
            self.delivered.fetch_add(rows, Ordering::Relaxed);
            Box::pin(async { Ok(SendResult::Ok) })
        }
    }

    fn flush(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn name(&self) -> &str {
        "partition-then-recover"
    }

    fn shutdown(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
}
