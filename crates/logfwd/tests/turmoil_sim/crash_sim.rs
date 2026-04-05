//! Crash consistency and checkpoint invariant simulation tests.
//!
//! These tests verify checkpoint ordering and crash recovery properties
//! using ObservableCheckpointStore with Arc-shared state for post-test
//! invariant inspection.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use logfwd::pipeline::Pipeline;
use logfwd_core::pipeline::SourceId;
use logfwd_test_utils::sinks::CountingSink;
use tokio_util::sync::CancellationToken;

use super::channel_input::ChannelInputSource;
use super::observable_checkpoint::ObservableCheckpointStore;

fn generate_json_lines(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("{{\"msg\":\"line {i}\",\"num\":{i}}}\n").into_bytes())
        .collect()
}

/// Test: crash on checkpoint flush with strong state inspection.
///
/// Invariant probed: at-least-once delivery guarantee.
/// When a crash is armed, the flush fails and pending checkpoint state
/// is lost. We verify:
/// 1. Data IS delivered to the sink (output succeeded independently of checkpoint)
/// 2. The crash actually happened (crash_count > 0)
/// 3. Checkpoint update history is monotonically increasing per source
/// 4. Subsequent flushes succeed (pipeline recovers from the crash)
#[test]
fn crash_checkpoint_consistency() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(30))
        .tick_duration(Duration::from_millis(1))
        .build();

    let delivered = Arc::new(AtomicU64::new(0));
    let delivered_clone = delivered.clone();

    let (store, handle) = ObservableCheckpointStore::new();
    let handle_clone = handle.clone();

    sim.client("pipeline", async move {
        let lines = generate_json_lines(20);
        let input = ChannelInputSource::new("test", SourceId(42), lines);
        let sink = CountingSink::new(delivered_clone);

        let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
        pipeline.set_batch_timeout(Duration::from_millis(10));
        pipeline.set_checkpoint_flush_interval(Duration::from_millis(50));
        let mut pipeline = pipeline
            .with_input("test", Box::new(input))
            .with_checkpoint_store(Box::new(store));

        // Arm the crash — first checkpoint flush will fail.
        handle_clone.arm_crash();

        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            sd.cancel();
        });

        pipeline.run_async(&shutdown).await.unwrap();
        Ok(())
    });

    sim.run().unwrap();

    // 1. Data was delivered to the sink despite the checkpoint crash.
    let count = delivered.load(Ordering::Relaxed);
    assert!(count > 0, "expected data delivered despite checkpoint crash, got 0");

    // 2. The crash actually happened — verify our crash injection worked.
    assert!(
        handle.crash_count() > 0,
        "expected at least 1 crash, but crash_count is 0 — crash injection may not have fired"
    );

    // 3. Checkpoint updates for source 42 are monotonically increasing.
    //    This verifies the PipelineMachine never commits a lower offset
    //    after a higher one.
    handle.assert_monotonic(42);

    // 4. Pipeline recovered — subsequent flushes succeeded after the crash.
    assert!(
        handle.flush_count() > 0,
        "expected successful flushes after crash recovery, but flush_count is 0"
    );
}

/// Test: multi-source checkpoint independence with invariant verification.
///
/// Invariant probed: per-source checkpoint independence.
/// Source 1 (10 lines) and Source 2 (5 lines) run concurrently.
/// Verifies:
/// 1. All 15 rows delivered
/// 2. Both sources have checkpoint updates
/// 3. Each source's checkpoint history is monotonic (independently)
/// 4. Source 2's checkpoint doesn't depend on Source 1's progress
#[test]
fn multi_source_independent_checkpoint() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(30))
        .tick_duration(Duration::from_millis(1))
        .build();

    let delivered = Arc::new(AtomicU64::new(0));
    let delivered_clone = delivered.clone();

    let (store, handle) = ObservableCheckpointStore::new();

    sim.client("pipeline", async move {
        let lines1 = generate_json_lines(10);
        let lines2 = generate_json_lines(5);
        let input1 = ChannelInputSource::new("source1", SourceId(1), lines1);
        let input2 = ChannelInputSource::new("source2", SourceId(2), lines2);
        let sink = CountingSink::new(delivered_clone);

        let mut pipeline = Pipeline::for_simulation("sim", Box::new(sink));
        pipeline.set_batch_timeout(Duration::from_millis(10));
        pipeline.set_checkpoint_flush_interval(Duration::from_millis(50));
        let mut pipeline = pipeline
            .with_input("source1", Box::new(input1))
            .with_input("source2", Box::new(input2))
            .with_checkpoint_store(Box::new(store));

        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            sd.cancel();
        });

        pipeline.run_async(&shutdown).await.unwrap();
        Ok(())
    });

    sim.run().unwrap();

    // 1. All 15 rows from both sources delivered.
    let count = delivered.load(Ordering::Relaxed);
    assert_eq!(count, 15, "expected all 15 rows from 2 sources, got {count}");

    // 2. Both sources have checkpoint updates.
    let s1_updates = handle.update_count(1);
    let s2_updates = handle.update_count(2);
    assert!(s1_updates > 0, "expected checkpoint updates for source 1, got 0");
    assert!(s2_updates > 0, "expected checkpoint updates for source 2, got 0");

    // 3. Each source's checkpoint history is independently monotonic.
    handle.assert_monotonic(1);
    handle.assert_monotonic(2);

    // 4. Both sources have durable checkpoints (flushes succeeded).
    assert!(
        handle.durable_offset(1).is_some(),
        "expected durable checkpoint for source 1"
    );
    assert!(
        handle.durable_offset(2).is_some(),
        "expected durable checkpoint for source 2"
    );
}
