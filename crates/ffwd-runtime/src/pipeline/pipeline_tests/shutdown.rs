/// Terminal held-ticket shutdown must close the input channel before
/// joining producers. Otherwise producers blocked on a full bounded channel
/// can keep shutdown stuck forever.
#[tokio::test(flavor = "multi_thread")]
async fn held_ticket_shutdown_does_not_deadlock_on_full_channel() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("held_ticket_full_channel.log");

    let mut data = String::new();
    for i in 0..200 {
        data.push_str(&format!(r#"{{"i":{i}}}"#));
        data.push('\n');
    }
    std::fs::write(&log_path, data.as_bytes()).unwrap();

    let yaml = single_pipeline_yaml(
        &format!("type: file\npath: \"{}\"\nformat: json", log_path.display()),
        "type: stdout\nformat: json",
    );

    let config = ffwd_config::Config::load_str(&yaml).unwrap();
    let pipe_cfg = &config.pipelines["default"];
    let mut pipeline = Pipeline::from_config("default", pipe_cfg, &test_meter(), None).unwrap();
    // A panic in the sink is converted into InternalFailure by the worker,
    // which maps to Hold and triggers terminal shutdown.
    pipeline = pipeline.with_sink(Box::new(PanicSink));
    pipeline.batch_target_bytes = 64;
    pipeline.batch_timeout = Duration::from_millis(10);
    pipeline.set_pool_drain_timeout(Duration::from_secs(1));

    let shutdown = CancellationToken::new();
    let result = tokio::time::timeout(Duration::from_secs(5), pipeline.run_async(&shutdown)).await;

    assert!(
        result.is_ok(),
        "held-ticket shutdown should close the channel and not deadlock"
    );
    assert!(
        result.unwrap().is_ok(),
        "pipeline should complete terminal held-ticket shutdown cleanly"
    );
}

/// Shutdown with a slow output must still drain all buffered data.
/// The output takes 50ms per batch, but shutdown should wait for
/// the drain to complete rather than dropping data.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_shutdown_drains_with_slow_output() {
    use std::sync::atomic::Ordering;

    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("slow_output.log");

    let mut data = String::new();
    for i in 0..50 {
        data.push_str(&format!(r#"{{"msg":"slow {}"}}"#, i));
        data.push('\n');
    }
    std::fs::write(&log_path, data.as_bytes()).unwrap();

    let yaml = single_pipeline_yaml(
        &format!("type: file\npath: \"{}\"\nformat: json", log_path.display()),
        "type: stdout\nformat: json",
    );

    let config = ffwd_config::Config::load_str(&yaml).unwrap();
    let pipe_cfg = &config.pipelines["default"];
    let mut pipeline = Pipeline::from_config("default", pipe_cfg, &test_meter(), None).unwrap();
    pipeline = pipeline.with_sink(Box::new(SlowSink {
        delay: Duration::from_millis(20),
    }));
    pipeline.batch_timeout = Duration::from_millis(30);

    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        sd.cancel();
    });

    pipeline.run_async(&shutdown).await.unwrap();

    let lines_in = pipeline
        .metrics
        .transform_in
        .lines_total
        .load(Ordering::Relaxed);
    assert!(
        lines_in >= 50,
        "slow output: all data should drain on shutdown, got {lines_in} (expected >= 50)"
    );
}

/// Emits one newline-terminated JSON line per `poll()` so every line becomes
/// its own `ProcessedBatch` when `batch_target_bytes` is tiny.
struct OneLinePerPollInput {
    lines: std::collections::VecDeque<Vec<u8>>,
}

impl OneLinePerPollInput {
    fn new(count: usize) -> Self {
        let lines = (0..count)
            .map(|i| format!(r#"{{"seq":{i}}}"#).into_bytes())
            .map(|mut line| {
                line.push(b'\n');
                line
            })
            .collect();
        Self { lines }
    }
}

impl InputSource for OneLinePerPollInput {
    fn poll(&mut self) -> io::Result<Vec<SourceEvent>> {
        Ok(self
            .lines
            .pop_front()
            .map(|line| {
                let accounted_bytes = line.len() as u64;
                vec![SourceEvent::Data {
                    bytes: Bytes::from(line),
                    source_id: None,
                    accounted_bytes,
                    cri_metadata: None,
                }]
            })
            .unwrap_or_default())
    }

    fn name(&self) -> &str {
        "one-line-per-poll"
    }

    fn health(&self) -> ComponentHealth {
        ComponentHealth::Healthy
    }
}

/// Blocks every send until `release` is cancelled, then counts delivered rows.
struct GatedCountingSink {
    release: CancellationToken,
    rows: Arc<std::sync::atomic::AtomicU64>,
}

impl Sink for GatedCountingSink {
    fn send_batch<'a>(
        &'a mut self,
        batch: &'a RecordBatch,
        _metadata: &'a BatchMetadata,
    ) -> Pin<Box<dyn Future<Output = SendResult> + Send + 'a>> {
        Box::pin(async move {
            self.release.cancelled().await;
            self.rows
                .fetch_add(batch.num_rows() as u64, Ordering::Relaxed);
            SendResult::Ok
        })
    }

    fn flush(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn name(&self) -> &str {
        "gated-counting"
    }

    fn shutdown(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
}

/// DrainIngress must not disable queued-input draining during a later shutdown.
///
/// Regression for #2716: the control-plane branch previously set
/// `should_drain_input_channel = false` on DrainIngress, so a later shutdown
/// dropped the input channel receiver with batches still queued in it.
///
/// The test is deterministic rather than timing-based:
/// 1. DrainIngress is queued before `run_async` starts. The run loop's biased
///    `select!` polls the control channel before the input channel, so it is
///    handled before any data batch.
/// 2. The sink blocks, and the single output worker has a one-slot queue, so
///    the run loop stalls in `pool.submit` and later batches pile up in the
///    bounded input channel.
/// 3. Once every line has been transformed and the channel holds batches,
///    shutdown is cancelled and only then is the sink released.
///
/// Every line must reach the sink. With the bug, the queued batches are lost.
#[tokio::test(flavor = "multi_thread")]
async fn drain_ingress_then_shutdown_delivers_queued_batches() {
    const LINES: u64 = 12;

    let dir = tempfile::tempdir().unwrap();
    // The config needs one input; this empty file never produces data. All
    // rows come from `OneLinePerPollInput` below.
    let empty_path = dir.path().join("unused.log");
    std::fs::write(&empty_path, b"").unwrap();
    let yaml = single_pipeline_yaml(
        &format!("type: file\npath: \"{}\"\nformat: json", empty_path.display()),
        "type: stdout\nformat: json",
    );
    let config = ffwd_config::Config::load_str(&yaml).unwrap();
    let pipe_cfg = &config.pipelines["default"];

    let release = CancellationToken::new();
    let delivered_rows = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let mut pipeline = Pipeline::from_config("default", pipe_cfg, &test_meter(), None).unwrap();
    // One line per batch: each poll yields one line and flushes immediately.
    pipeline.batch_target_bytes = 1;
    pipeline.batch_timeout = Duration::from_millis(5);
    pipeline.set_pool_drain_timeout(Duration::from_secs(5));
    pipeline = pipeline
        .with_input("one-line", Box::new(OneLinePerPollInput::new(LINES as usize)))
        .with_sink(Box::new(GatedCountingSink {
            release: release.clone(),
            rows: Arc::clone(&delivered_rows),
        }));

    pipeline
        .clone_control_sender()
        .send(ControlMessage::DrainIngress)
        .expect("control channel open");

    let metrics = Arc::clone(&pipeline.metrics);
    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();
    let gate = release.clone();
    let driver = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let transformed = metrics.transform_in.lines_total.load(Ordering::Relaxed);
            let queued = metrics.channel_depth.load(Ordering::Relaxed);
            if transformed == LINES && queued > 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                sd.cancel();
                gate.cancel();
                panic!(
                    "setup did not queue batches in the input channel \
                     (transformed={transformed}, queued={queued})"
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let queued = metrics.channel_depth.load(Ordering::Relaxed);
        sd.cancel();
        gate.cancel();
        queued
    });

    let result = tokio::time::timeout(Duration::from_secs(20), pipeline.run_async(&shutdown))
        .await
        .expect("pipeline shutdown should not hang");
    result.expect("pipeline run should succeed");
    let queued_at_shutdown = driver.await.expect("driver task should not panic");

    assert!(
        queued_at_shutdown > 0,
        "test setup must have batches queued in the input channel at shutdown"
    );
    assert_eq!(
        delivered_rows.load(Ordering::Relaxed),
        LINES,
        "shutdown after DrainIngress must deliver the {queued_at_shutdown} batches \
         queued in the input channel"
    );
}

/// After all file data is read, shutdown must still drain everything.
/// The file tailer keeps polling after EOF (it's a tailer, not a
/// reader), so the pipeline only exits via shutdown signal.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_pipeline_processes_all_data_before_shutdown() {
    use std::sync::atomic::Ordering;

    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("eof_test.log");

    let mut data = String::new();
    for i in 0..30 {
        data.push_str(&format!(r#"{{"eof":{i}}}"#));
        data.push('\n');
    }
    std::fs::write(&log_path, data.as_bytes()).unwrap();

    let yaml = single_pipeline_yaml(
        &format!("type: file\npath: \"{}\"\nformat: json", log_path.display()),
        "type: stdout\nformat: json",
    );

    let config = ffwd_config::Config::load_str(&yaml).unwrap();
    let pipe_cfg = &config.pipelines["default"];
    let mut pipeline = Pipeline::from_config("default", pipe_cfg, &test_meter(), None).unwrap();
    pipeline = pipeline.with_sink(Box::new(DevNullSink));
    pipeline.batch_timeout = Duration::from_millis(20);

    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();

    // Wait for data to be processed, then shut down.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        sd.cancel();
    });

    pipeline.run_async(&shutdown).await.unwrap();

    let lines_in = pipeline
        .metrics
        .transform_in
        .lines_total
        .load(Ordering::Relaxed);
    assert!(
        lines_in >= 30,
        "all file data should be processed before shutdown, got {lines_in} (expected >= 30)"
    );
}

/// Shutdown with zero data should complete instantly without errors.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_shutdown_empty_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("empty.log");
    std::fs::write(&log_path, b"").unwrap();

    let yaml = single_pipeline_yaml(
        &format!("type: file\npath: \"{}\"\nformat: json", log_path.display()),
        "type: stdout\nformat: json",
    );

    let config = ffwd_config::Config::load_str(&yaml).unwrap();
    let pipe_cfg = &config.pipelines["default"];
    let mut pipeline = Pipeline::from_config("default", pipe_cfg, &test_meter(), None).unwrap();
    pipeline = pipeline.with_sink(Box::new(DevNullSink));

    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        sd.cancel();
    });

    let result = tokio::time::timeout(Duration::from_secs(2), pipeline.run_async(&shutdown)).await;

    assert!(result.is_ok(), "empty pipeline should shut down cleanly");
    assert!(result.unwrap().is_ok());
}

/// Immediate shutdown (cancel before run_async) must not panic or hang.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_immediate_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("immediate.log");
    std::fs::write(&log_path, b"{\"a\":1}\n").unwrap();

    let yaml = single_pipeline_yaml(
        &format!("type: file\npath: \"{}\"\nformat: json", log_path.display()),
        "type: stdout\nformat: json",
    );

    let config = ffwd_config::Config::load_str(&yaml).unwrap();
    let pipe_cfg = &config.pipelines["default"];
    let mut pipeline = Pipeline::from_config("default", pipe_cfg, &test_meter(), None).unwrap();
    pipeline = pipeline.with_sink(Box::new(DevNullSink));

    let shutdown = CancellationToken::new();
    // Cancel immediately, before even starting.
    shutdown.cancel();

    let result = tokio::time::timeout(Duration::from_secs(2), pipeline.run_async(&shutdown)).await;

    assert!(result.is_ok(), "immediate shutdown must not hang");
    assert!(result.unwrap().is_ok(), "immediate shutdown must not error");
}

/// Output errors must not crash the pipeline. Retry/control-plane
/// failures are surfaced as output errors without advancing checkpoints.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_output_errors_do_not_crash() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("output_err.log");

    let mut data = String::new();
    for i in 0..100 {
        data.push_str(&format!(r#"{{"err":{i}}}"#));
        data.push('\n');
    }
    std::fs::write(&log_path, data.as_bytes()).unwrap();

    let yaml = single_pipeline_yaml(
        &format!("type: file\npath: \"{}\"\nformat: json", log_path.display()),
        "type: stdout\nformat: json",
    );

    let config = ffwd_config::Config::load_str(&yaml).unwrap();
    let pipe_cfg = &config.pipelines["default"];
    let mut pipeline = Pipeline::from_config("default", pipe_cfg, &test_meter(), None).unwrap();
    // FailingSink fails the first N batches, then succeeds.
    pipeline = pipeline.with_sink(Box::new(FailingSink {
        fail_count: 3,
        calls: 0,
    }));
    pipeline.batch_timeout = Duration::from_millis(20);

    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        sd.cancel();
    });

    let result = pipeline.run_async(&shutdown).await;
    assert!(result.is_ok(), "output errors should not crash pipeline");

    // Data should still flow through transform even though output
    // failed on some batches.
    let lines_in = pipeline
        .metrics
        .transform_in
        .lines_total
        .load(Ordering::Relaxed);
    assert!(
        lines_in > 0,
        "data should flow through transform despite output errors"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_async_fanout_output_errors_do_not_count_as_permanent_drops() {
    // This test verifies that a persistently-failing output does not
    // crash the pipeline and is not misclassified as a permanent drop.
    //
    // With the async worker pool, retries are built into the pool. An
    // always-failing sink causes the pool to exhaust retries and return a
    // non-delivered AckItem outcome that should HOLD the checkpoint seam.
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("fanout_output_err.log");

    let mut data = String::new();
    for i in 0..100 {
        data.push_str(&format!(r#"{{"err":{i}}}"#));
        data.push('\n');
    }
    std::fs::write(&log_path, data.as_bytes()).unwrap();

    let yaml = format!(
        r#"
pipelines:
  default:
    inputs:
      - type: file
        path: "{}"
        format: json
    outputs:
      - type: stdout
        format: json
        name: always-failing
"#,
        log_path.display()
    );

    let config = ffwd_config::Config::load_str(&yaml).unwrap();
    let pipe_cfg = &config.pipelines["default"];
    let mut pipeline = Pipeline::from_config("default", pipe_cfg, &test_meter(), None).unwrap();
    // always-failing: fails every call — workers retry indefinitely and the
    // pipeline must hold checkpoint progress instead of rejecting it.
    pipeline = pipeline.with_sink(Box::new(FailingSink::new(u32::MAX)));
    pipeline.batch_timeout = Duration::from_millis(20);
    // Short drain timeout so pool cancels workers promptly on shutdown.
    pipeline.set_pool_drain_timeout(Duration::from_secs(1));

    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();

    tokio::spawn(async move {
        // Give workers time to start retrying.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        sd.cancel();
    });

    let result = pipeline.run_async(&shutdown).await;
    assert!(
        result.is_ok(),
        "fanout output errors should not crash pipeline"
    );

    let lines_in = pipeline
        .metrics
        .transform_in
        .lines_total
        .load(Ordering::Relaxed);
    assert!(
        lines_in > 0,
        "data should still reach transform despite output failure"
    );

    let dropped = pipeline
        .metrics
        .dropped_batches_total
        .load(Ordering::Relaxed);
    assert_eq!(
        dropped, 0,
        "retry/control-plane failures should not count as permanent drops"
    );
}

/// Rapid repeated shutdown signals must not cause panics.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_repeated_shutdown_signals() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("repeat_shutdown.log");

    let mut data = String::new();
    for i in 0..50 {
        data.push_str(&format!(r#"{{"rep":{i}}}"#));
        data.push('\n');
    }
    std::fs::write(&log_path, data.as_bytes()).unwrap();

    let yaml = single_pipeline_yaml(
        &format!("type: file\npath: \"{}\"\nformat: json", log_path.display()),
        "type: stdout\nformat: json",
    );

    let config = ffwd_config::Config::load_str(&yaml).unwrap();
    let pipe_cfg = &config.pipelines["default"];
    let mut pipeline = Pipeline::from_config("default", pipe_cfg, &test_meter(), None).unwrap();
    pipeline = pipeline.with_sink(Box::new(DevNullSink));
    pipeline.batch_timeout = Duration::from_millis(20);

    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();

    // Cancel many times — CancellationToken should handle this gracefully.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        sd.cancel();
        sd.cancel();
        sd.cancel();
    });

    let result = tokio::time::timeout(Duration::from_secs(2), pipeline.run_async(&shutdown)).await;

    assert!(result.is_ok(), "repeated shutdown must not hang");
    assert!(result.unwrap().is_ok());
}

/// Large batch that exceeds batch_target_bytes on a single line
/// must still be processed and not cause channel issues.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_oversized_single_line() {
    use std::sync::atomic::Ordering;

    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("big_line.log");

    // Single line with a large value (~10KB).
    let big_value = "x".repeat(10_000);
    let data = format!(r#"{{"big":"{}"}}"#, big_value);
    std::fs::write(&log_path, format!("{data}\n")).unwrap();

    let yaml = single_pipeline_yaml(
        &format!("type: file\npath: \"{}\"\nformat: json", log_path.display()),
        "type: stdout\nformat: json",
    );

    let config = ffwd_config::Config::load_str(&yaml).unwrap();
    let pipe_cfg = &config.pipelines["default"];
    let mut pipeline = Pipeline::from_config("default", pipe_cfg, &test_meter(), None).unwrap();
    pipeline = pipeline.with_sink(Box::new(DevNullSink));
    pipeline.batch_target_bytes = 1024; // Much smaller than the line
    pipeline.batch_timeout = Duration::from_millis(20);

    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        sd.cancel();
    });

    pipeline.run_async(&shutdown).await.unwrap();

    let lines_in = pipeline
        .metrics
        .transform_in
        .lines_total
        .load(Ordering::Relaxed);
    assert!(
        lines_in >= 1,
        "oversized line should still be processed, got {lines_in}"
    );
}
