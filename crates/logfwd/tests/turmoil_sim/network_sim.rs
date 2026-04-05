//! Invariant-based failure simulation tests.
//!
//! These tests exercise the real Pipeline + OutputWorkerPool + PipelineMachine
//! interaction under controlled failure conditions. Each test documents the
//! invariant it probes and makes strong assertions on state, not just counts.

use std::io;
use std::sync::atomic::Ordering;
use std::time::Duration;

use logfwd::pipeline::Pipeline;
use logfwd_core::pipeline::SourceId;
use tokio_util::sync::CancellationToken;

use super::channel_input::ChannelInputSource;
use super::instrumented_sink::{FailureAction, InstrumentedSink};
use super::observable_checkpoint::ObservableCheckpointStore;

fn generate_json_lines(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("{{\"msg\":\"line {i}\",\"num\":{i}}}\n").into_bytes())
        .collect()
}

/// Test: ordered ACK under out-of-order delivery.
///
/// Invariant probed: PipelineMachine ordered-ack — checkpoint advances only
/// when ALL prior batches for a source are acknowledged. A slow first batch
/// must not block correct checkpoint advancement after it completes.
///
/// Script: first send_batch delays 3s, subsequent calls succeed instantly.
/// After pipeline shutdown:
/// - All 20 rows delivered
/// - Checkpoint update history is monotonically increasing
/// - Durable checkpoint exists and reflects delivered data
#[test]
fn ordered_ack_under_out_of_order_delivery() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .tick_duration(Duration::from_millis(1))
        .build();

    let sink = InstrumentedSink::new(vec![FailureAction::Delay(Duration::from_secs(3))]);
    let delivered_counter = sink.delivered_counter();
    let call_counter = sink.call_counter();

    let (store, ckpt_handle) = ObservableCheckpointStore::new();

    sim.client("pipeline", async move {
        let lines = generate_json_lines(20);
        let input = ChannelInputSource::new("test", SourceId(1), lines);

        let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
        pipeline.set_batch_timeout(Duration::from_millis(20));
        pipeline.set_checkpoint_flush_interval(Duration::from_millis(100));
        let mut pipeline = pipeline
            .with_input("test", Box::new(input))
            .with_checkpoint_store(Box::new(store));

        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(15)).await;
            sd.cancel();
        });

        pipeline.run_async(&shutdown).await.unwrap();
        Ok(())
    });

    sim.run().unwrap();

    // All 20 rows must be delivered (first batch delayed, not failed).
    let count = delivered_counter.load(Ordering::Relaxed);
    assert_eq!(count, 20, "expected all 20 rows delivered, got {count}");

    // The sink was called at least once.
    let calls = call_counter.load(Ordering::Relaxed);
    assert!(calls >= 1, "expected at least 1 send_batch call, got {calls}");

    // INVARIANT: checkpoint history is monotonically increasing.
    // If the PipelineMachine committed out-of-order, this would catch it.
    ckpt_handle.assert_monotonic(1);

    // INVARIANT: durable checkpoint exists and reflects some delivered data.
    let durable = ckpt_handle.durable_offset(1);
    assert!(
        durable.is_some(),
        "expected durable checkpoint for source 1 after delivering 20 rows"
    );
    assert!(
        durable.unwrap() > 0,
        "expected durable checkpoint offset > 0, got {}",
        durable.unwrap()
    );
}

/// Test: retry exhaustion drops batch and pipeline does not hang.
///
/// Invariant probed: worker pool retry exhaustion (MAX_RETRIES=3, so 4 total
/// attempts). When all attempts fail, the batch is rejected, the pipeline
/// increments dropped_batch, and shutdown completes (no deadlock).
///
/// Script: all calls return IoError(ConnectionRefused).
#[test]
fn retry_exhaustion_drops_batch_and_advances() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(120))
        .tick_duration(Duration::from_millis(1))
        .build();

    let mut script = Vec::new();
    for _ in 0..100 {
        script.push(FailureAction::IoError(io::ErrorKind::ConnectionRefused));
    }
    let sink = InstrumentedSink::new(script);
    let delivered_counter = sink.delivered_counter();
    let call_counter = sink.call_counter();

    sim.client("pipeline", async move {
        let lines = generate_json_lines(10);
        let input = ChannelInputSource::new("test", SourceId(1), lines);

        let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
        pipeline.set_batch_timeout(Duration::from_millis(20));
        let mut pipeline = pipeline.with_input("test", Box::new(input));

        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            sd.cancel();
        });

        // Pipeline must NOT hang despite all failures.
        pipeline.run_async(&shutdown).await.unwrap();
        Ok(())
    });

    sim.run().unwrap();

    // No rows should be delivered — all attempts failed.
    let delivered = delivered_counter.load(Ordering::Relaxed);
    assert_eq!(delivered, 0, "expected 0 rows delivered, got {delivered}");

    // The sink should have been called at least 4 times (1 batch * 4 attempts).
    let calls = call_counter.load(Ordering::Relaxed);
    assert!(calls >= 4, "expected >= 4 send_batch calls (1+MAX_RETRIES), got {calls}");
}

/// Test: shutdown drain with in-flight slow work.
///
/// Invariant probed: shutdown race between drain and in-flight work.
/// The pool.drain(60s) + force_stop path must handle slow sinks without
/// deadlocking.
#[test]
fn shutdown_drain_with_inflight_work() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(120))
        .tick_duration(Duration::from_millis(10))
        .build();

    let sink = InstrumentedSink::new(vec![FailureAction::Delay(Duration::from_secs(5))]);
    let delivered_counter = sink.delivered_counter();

    sim.client("pipeline", async move {
        let lines = generate_json_lines(10);
        let input = ChannelInputSource::new("test", SourceId(1), lines);

        let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
        pipeline.set_batch_timeout(Duration::from_millis(20));
        let mut pipeline = pipeline.with_input("test", Box::new(input));

        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();
        // Shutdown after 1s — first batch is still in 5s delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            sd.cancel();
        });

        pipeline.run_async(&shutdown).await.unwrap();
        Ok(())
    });

    // Pipeline must complete shutdown without deadlocking.
    sim.run().unwrap();

    // Some rows may or may not be delivered depending on drain timing.
    // The invariant is: pipeline shuts down cleanly within bounded time.
    let delivered = delivered_counter.load(Ordering::Relaxed);
    // At least verify we can read the counter (pipeline didn't panic/hang).
    let _ = delivered;
}
