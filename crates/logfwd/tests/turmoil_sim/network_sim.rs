//! Network failure simulation tests.
//!
//! These tests exercise the pipeline under failure modes that are nearly
//! impossible to test reliably with real networking: partitions, retry
//! storms, and high-latency outputs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use logfwd::pipeline::Pipeline;
use logfwd_core::pipeline::SourceId;
use tokio_util::sync::CancellationToken;

use super::channel_input::ChannelInputSource;
use super::failure_sinks::{HighLatencySink, PartitionThenRecoverSink, RetryThenSucceedSink};

fn generate_json_lines(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("{{\"msg\":\"line {i}\",\"num\":{i}}}\n").into_bytes())
        .collect()
}

/// Test: output returns IO errors (connection refused) for first 3 batches,
/// then recovers. Pipeline should not panic or hang; data sent after recovery
/// should be delivered.
///
/// This simulates a network partition that repairs itself. The pipeline's
/// worker pool should reject batches during the partition (incrementing
/// dropped_batch metrics) and resume normal delivery after recovery.
#[test]
fn pipeline_survives_network_partition() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(30))
        .tick_duration(Duration::from_millis(10))
        .build();

    let delivered = Arc::new(AtomicU64::new(0));
    let delivered_clone = delivered.clone();

    sim.client("pipeline", async move {
        // 20 lines — enough for multiple batches at small target
        let lines = generate_json_lines(20);
        let input = ChannelInputSource::new("test", SourceId(1), lines);

        // Fail first 3 send_batch calls with connection refused, then succeed
        let sink = PartitionThenRecoverSink::new(3, delivered_clone);

        let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
        pipeline.set_batch_timeout(Duration::from_millis(20));
        let mut pipeline = pipeline.with_input("test", Box::new(input));

        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            sd.cancel();
        });

        // Pipeline should NOT panic even with IO errors from the sink
        pipeline.run_async(&shutdown).await.unwrap();
        Ok(())
    });

    sim.run().unwrap();

    // Some rows should have been delivered after the partition healed.
    // We can't assert exactly 20 because batches during partition are dropped.
    let count = delivered.load(Ordering::Relaxed);
    assert!(
        count > 0,
        "expected some rows delivered after partition recovery, got 0"
    );
}

/// Test: output returns RetryAfter(100ms) for first 2 calls, then succeeds.
/// The worker pool should retry after the delay and eventually deliver.
///
/// Under Turmoil's simulated time, the 100ms retry delay advances instantly,
/// so this test runs in milliseconds of real time.
#[test]
fn retry_on_503_then_succeeds() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(30))
        .tick_duration(Duration::from_millis(1))
        .build();

    let delivered = Arc::new(AtomicU64::new(0));
    let delivered_clone = delivered.clone();

    sim.client("pipeline", async move {
        let lines = generate_json_lines(10);
        let input = ChannelInputSource::new("test", SourceId(1), lines);

        // Return RetryAfter for first 2 calls, then succeed
        let sink =
            RetryThenSucceedSink::new(2, Duration::from_millis(100), delivered_clone);

        let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
        pipeline.set_batch_timeout(Duration::from_millis(20));
        let mut pipeline = pipeline.with_input("test", Box::new(input));

        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            sd.cancel();
        });

        pipeline.run_async(&shutdown).await.unwrap();
        Ok(())
    });

    sim.run().unwrap();

    let count = delivered.load(Ordering::Relaxed);
    assert_eq!(
        count, 10,
        "expected all 10 rows delivered after retry recovery, got {count}"
    );
}

/// Test: output has 500ms latency per batch. Pipeline should still
/// accumulate and flush subsequent batches while waiting for output.
///
/// This tests that the async worker pool doesn't stall the accumulator.
/// Under Turmoil, 500ms advances instantly, but the pipeline event loop
/// must still interleave accumulation with output completion.
#[test]
fn high_latency_does_not_stall_batching() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .tick_duration(Duration::from_millis(10))
        .build();

    let delivered = Arc::new(AtomicU64::new(0));
    let delivered_clone = delivered.clone();

    sim.client("pipeline", async move {
        let lines = generate_json_lines(30);
        let input = ChannelInputSource::new("test", SourceId(1), lines);

        // 500ms latency per send — simulates overloaded server
        let sink = HighLatencySink::new(Duration::from_millis(500), delivered_clone);

        let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
        pipeline.set_batch_timeout(Duration::from_millis(50));
        let mut pipeline = pipeline.with_input("test", Box::new(input));

        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            sd.cancel();
        });

        pipeline.run_async(&shutdown).await.unwrap();
        Ok(())
    });

    sim.run().unwrap();

    let count = delivered.load(Ordering::Relaxed);
    assert_eq!(
        count, 30,
        "expected all 30 rows delivered despite high latency, got {count}"
    );
}
