//! Turmoil simulation tests for the logfwd pipeline.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use logfwd::pipeline::Pipeline;
use logfwd_core::pipeline::SourceId;
use logfwd_test_utils::sinks::CountingSink;
use tokio_util::sync::CancellationToken;

use super::channel_input::ChannelInputSource;

/// Generate N JSON lines for testing.
fn generate_json_lines(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("{{\"msg\":\"line {i}\",\"num\":{i}}}\n").into_bytes())
        .collect()
}

#[test]
fn happy_path_all_data_delivered() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(30))
        .tick_duration(Duration::from_millis(10))
        .build();

    let delivered = Arc::new(AtomicU64::new(0));
    let delivered_clone = delivered.clone();

    sim.client("pipeline", async move {
        let lines = generate_json_lines(50);
        let input = ChannelInputSource::new("test", SourceId(1), lines);
        let sink = CountingSink::new(delivered_clone);

        let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
        pipeline.set_batch_timeout(Duration::from_millis(20));
        let pipeline = pipeline.with_input("test", Box::new(input));
        let mut pipeline = pipeline;

        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();

        // Cancel after enough time for all data to flow through
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            sd.cancel();
        });

        pipeline.run_async(&shutdown).await.unwrap();
        Ok(())
    });

    sim.run().unwrap();

    let count = delivered.load(Ordering::Relaxed);
    assert!(
        count > 0,
        "expected at least some rows delivered, got {count}"
    );
    assert_eq!(count, 50, "expected all 50 rows delivered, got {count}");
}

#[test]
fn graceful_shutdown_drains_buffered_data() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(30))
        .tick_duration(Duration::from_millis(10))
        .build();

    let delivered = Arc::new(AtomicU64::new(0));
    let delivered_clone = delivered.clone();

    sim.client("pipeline", async move {
        let lines = generate_json_lines(10);
        let input = ChannelInputSource::new("test", SourceId(1), lines);
        let sink = CountingSink::new(delivered_clone);

        let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
        // Large timeout so data sits in buffer
        pipeline.set_batch_timeout(Duration::from_secs(60));
        let pipeline = pipeline.with_input("test", Box::new(input));
        let mut pipeline = pipeline;

        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();

        // Cancel quickly -- data should still be drained
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            sd.cancel();
        });

        pipeline.run_async(&shutdown).await.unwrap();
        Ok(())
    });

    sim.run().unwrap();

    let count = delivered.load(Ordering::Relaxed);
    assert_eq!(
        count, 10,
        "expected all 10 rows drained on shutdown, got {count}"
    );
}

#[test]
fn batch_flushes_on_timeout() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(30))
        .tick_duration(Duration::from_millis(1))
        .build();

    let delivered = Arc::new(AtomicU64::new(0));
    let delivered_clone = delivered.clone();
    // A second clone to check mid-test from within the client.
    let mid_test_check = delivered.clone();

    sim.client("pipeline", async move {
        // 5 small lines -- well below batch_target_bytes
        let lines = generate_json_lines(5);
        let input = ChannelInputSource::new("test", SourceId(1), lines);
        let sink = CountingSink::new(delivered_clone);

        let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
        pipeline.set_batch_timeout(Duration::from_millis(50));
        let pipeline = pipeline.with_input("test", Box::new(input));
        let mut pipeline = pipeline;

        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();

        // Mid-test assertion: verify data is delivered by the timeout flush
        // BEFORE the shutdown signal fires.
        let mid = mid_test_check.clone();
        tokio::spawn(async move {
            // Wait long enough for the 50ms batch timeout to fire and
            // for data to flow through the pipeline (but well before the
            // 2s shutdown timer).
            tokio::time::sleep(Duration::from_millis(500)).await;
            let count = mid.load(Ordering::Relaxed);
            assert!(
                count > 0,
                "mid-test: expected data delivered by batch timeout flush \
                 before shutdown, got 0"
            );
        });

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            sd.cancel();
        });

        pipeline.run_async(&shutdown).await.unwrap();
        Ok(())
    });

    sim.run().unwrap();

    let count = delivered.load(Ordering::Relaxed);
    assert_eq!(
        count, 5,
        "expected timeout flush to deliver all 5 rows, got {count}"
    );
}
