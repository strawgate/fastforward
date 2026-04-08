use std::sync::Arc;
use std::time::Duration;

use arrow::record_batch::RecordBatch;
use backon::{BackoffBuilder, ExponentialBuilder};
use tokio::sync::mpsc;
use tracing::Instrument;

use logfwd_output::BatchMetadata;
use logfwd_output::sink::{OutputHealthEvent, SendResult, Sink};

use super::pool::{OutputHealthTracker, WorkerConfig, WorkerSlotCleanup};
use super::types::{AckItem, DeliveryOutcome, WorkItem, WorkerMsg, bound_rejection_reason};

// Worker task
// ---------------------------------------------------------------------------

/// Long-lived tokio task that owns one `Sink` and processes `WorkItem`s.
///
/// Exits when:
/// - The `rx` channel is closed (pool dropped the sender).
/// - No item arrives within `idle_timeout` (self-terminate to free connection).
/// - The `cancel` token is fired (hard shutdown).
pub(super) async fn worker_task(
    id: usize,
    mut sink: Box<dyn Sink>,
    mut rx: mpsc::Receiver<WorkerMsg>,
    ack_tx: mpsc::UnboundedSender<AckItem>,
    cfg: WorkerConfig,
) {
    let WorkerConfig {
        idle_timeout,
        cancel,
        max_retry_delay,
        metrics,
        output_health,
    } = cfg;
    let _slot_cleanup = WorkerSlotCleanup {
        output_health: Arc::clone(&output_health),
        worker_id: id,
    };
    loop {
        tokio::select! {
            biased; // check cancel first
            () = cancel.cancelled() => break,
            msg = recv_with_idle_timeout(&mut rx, idle_timeout) => {
                match msg {
                    None => break, // idle timeout or channel closed
                    Some(WorkerMsg::Shutdown) => break,
                    Some(WorkerMsg::Work(item)) => {
                        let WorkItem {
                            batch,
                            metadata,
                            tickets,
                            num_rows,
                            submitted_at,
                            scan_ns,
                            transform_ns,
                            batch_id,
                            span,
                        } = item;
                        let queue_wait_ns = submitted_at.elapsed().as_nanos() as u64;
                        span.record("queue_wait_ns", queue_wait_ns);
                        // Record which worker picked up this batch for the live dashboard.
                        let now_ns = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64;
                        metrics.assign_worker_to_active_batch(batch_id, id, now_ns);
                        let output_span = tracing::info_span!(
                            parent: &span, "output",
                            worker_id = id,
                            send_ns   = tracing::field::Empty,
                            recv_ns   = tracing::field::Empty,
                            took_ms   = tracing::field::Empty,
                            retries   = tracing::field::Empty,
                            req_bytes = tracing::field::Empty,
                            cmp_bytes = tracing::field::Empty,
                            resp_bytes = tracing::field::Empty,
                        );
                        let (outcome, send_latency_ns, retries) =
                            process_item(id, &mut *sink, &output_health, batch, &metadata, max_retry_delay)
                        .instrument(output_span.clone())
                        .await;
                        output_span.record("retries", retries);
                        output_span.record("send_ns", send_latency_ns);
                        let output_ns = submitted_at.elapsed().as_nanos() as u64 - queue_wait_ns;
                        // Remove from active_batches immediately — don't wait for the pipeline's
                        // ack select loop, which can be starved by flush_batch.await blocking.
                        metrics.finish_active_batch(batch_id);
                        drop(span);
                        let ack = AckItem {
                            tickets,
                            outcome,
                            num_rows,
                            submitted_at,
                            scan_ns,
                            transform_ns,
                            output_ns,
                            queue_wait_ns,
                            send_latency_ns,
                            batch_id,
                            output_name: sink.name().to_string(),
                        };
                        if let Err(send_err) = ack_tx.send(ack) {
                            let lost_ack = send_err.0;
                            tracing::warn!(
                                worker_id = id,
                                num_rows = lost_ack.num_rows,
                                "outcome" = ?lost_ack.outcome,
                                "worker: ack channel closed, ack lost"
                            );
                        }
                    }
                }
            }
        }
    }
    // Always shut the sink down when the worker exits so resources are flushed
    // and released. Health transitions for pipeline drain are driven by the
    // pool-level drain path; idle worker expiry should not make outputs appear
    // permanently unready because the pool can respawn workers on demand.
    if let Err(e) = sink.shutdown().await {
        tracing::error!(worker_id = id, error = %e, "worker_pool: sink shutdown failed");
        metrics.output_error(sink.name());
    }
}

/// Receive with idle timeout — returns `None` on timeout or channel close.
pub(super) async fn recv_with_idle_timeout(
    rx: &mut mpsc::Receiver<WorkerMsg>,
    idle_timeout: Duration,
) -> Option<WorkerMsg> {
    tokio::time::timeout(idle_timeout, rx.recv())
        .await
        .ok() // Err = timed out → None
        .flatten() // None = channel closed → None
}

/// Process one batch with retry on `RetryAfter` and server errors.
///
/// Returns `(outcome, send_latency_ns, retries)` where `send_latency_ns` is
/// cumulative wall time inside `sink.send_batch()` across all attempts
/// (excludes backoff sleep).
pub(super) async fn process_item(
    worker_id: usize,
    sink: &mut dyn Sink,
    output_health: &OutputHealthTracker,
    batch: RecordBatch,
    metadata: &BatchMetadata,
    max_retry_delay: Duration,
) -> (DeliveryOutcome, u64, usize) {
    sink.begin_batch();

    const MAX_RETRIES: usize = 3; // 1 initial + 3 retries = 4 total attempts
    const BATCH_TIMEOUT_SECS: u64 = 60;

    let mut backoff = ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(100))
        .with_max_delay(max_retry_delay)
        .with_factor(2.0)
        .with_max_times(MAX_RETRIES)
        .with_jitter()
        .build();

    let mut send_latency_ns: u64 = 0;
    let mut retries_count = 0;

    loop {
        // Hard per-batch timeout: prevents one slow/broken batch from
        // tying up the worker indefinitely.
        let send_start = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(BATCH_TIMEOUT_SECS),
            sink.send_batch(&batch, metadata),
        )
        .await;
        send_latency_ns += send_start.elapsed().as_nanos() as u64;

        match result {
            Err(_elapsed) => {
                tracing::error!(
                    worker_id,
                    timeout_secs = BATCH_TIMEOUT_SECS,
                    "worker_pool: batch send timed out"
                );
                output_health.apply_worker_event(worker_id, OutputHealthEvent::FatalFailure);
                return (DeliveryOutcome::TimedOut, send_latency_ns, retries_count);
            }
            Ok(SendResult::Ok) => {
                output_health.apply_worker_event(worker_id, OutputHealthEvent::DeliverySucceeded);
                return (DeliveryOutcome::Delivered, send_latency_ns, retries_count);
            }
            Ok(SendResult::Rejected(reason)) => {
                tracing::warn!(worker_id, %reason, "worker_pool: batch rejected");
                output_health.apply_worker_event(worker_id, OutputHealthEvent::FatalFailure);
                return (
                    DeliveryOutcome::Rejected {
                        reason: bound_rejection_reason(reason),
                    },
                    send_latency_ns,
                    retries_count,
                );
            }
            Ok(SendResult::RetryAfter(retry_dur)) => {
                // Server specified delay — consume a backoff slot but use
                // the server's delay (capped at max_retry_delay).
                if backoff.next().is_none() {
                    tracing::error!(
                        worker_id,
                        max_retries = MAX_RETRIES,
                        "worker_pool: RetryAfter exceeded max retries"
                    );
                    output_health.apply_worker_event(worker_id, OutputHealthEvent::FatalFailure);
                    return (
                        DeliveryOutcome::RetryExhausted,
                        send_latency_ns,
                        retries_count,
                    );
                }
                retries_count += 1;
                let sleep_for = retry_dur.min(max_retry_delay);
                output_health.apply_worker_event(worker_id, OutputHealthEvent::Retrying);
                tracing::warn!(worker_id, ?sleep_for, "worker_pool: rate-limited, retrying");
                tokio::time::sleep(sleep_for).await;
            }
            Ok(SendResult::IoError(e)) => match backoff.next() {
                Some(delay) => {
                    retries_count += 1;
                    output_health.apply_worker_event(worker_id, OutputHealthEvent::Retrying);
                    tracing::warn!(
                        worker_id,
                        sleep_ms = delay.as_millis() as u64,
                        error = %e,
                        "worker_pool: transient error, retrying with jitter"
                    );
                    tokio::time::sleep(delay).await;
                }
                None => {
                    tracing::error!(
                        worker_id,
                        max_retries = MAX_RETRIES,
                        error = %e,
                        "worker_pool: gave up after retries"
                    );
                    output_health.apply_worker_event(worker_id, OutputHealthEvent::FatalFailure);
                    return (
                        DeliveryOutcome::RetryExhausted,
                        send_latency_ns,
                        retries_count,
                    );
                }
            },
            // Future SendResult variants (#[non_exhaustive]) — treat as failure.
            Ok(_) => {
                output_health.apply_worker_event(worker_id, OutputHealthEvent::FatalFailure);
                return (
                    DeliveryOutcome::InternalFailure,
                    send_latency_ns,
                    retries_count,
                );
            }
        }
    }
}
