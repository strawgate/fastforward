// ---------------------------------------------------------------------------
// Shutdown retry grace window
//
// Why not typestate? The window has three states (not opened, open, closed),
// but it is shared: the pool, every worker and the pipeline's shutdown link
// task hold clones, and any of them may open it concurrently from different
// tasks. Each holder keeps a single binding for its whole life, so the state
// cannot move between types. The transitions are one-way (an atomic swap for
// "opened", a CancellationToken for "closed"), so neither can be undone, and
// the protocol is modeled in tla/DeliveryRetry.tla (BeginShutdown,
// StopRetrying, StopOnlyAfterShutdown).
// ---------------------------------------------------------------------------

/// Default time workers keep retrying failed sends after shutdown begins.
pub const DEFAULT_SHUTDOWN_RETRY_GRACE: Duration = Duration::from_secs(10);

/// Bounded retry window that opens when shutdown begins.
///
/// Before shutdown, workers retry failed sends forever. Once
/// [`ShutdownRetryGrace::begin`] is called, workers keep retrying with
/// their normal backoff for `grace`, so a brief output blip during a
/// restart still delivers. When the window closes, the stop token fires:
/// a worker that is backing off gives up at once, and later failures are
/// not retried. Those batches resolve as [`DeliveryOutcome::PoolClosed`],
/// so their checkpoints are held and they replay on the next start.
///
/// Clones share state, so a task that does not hold the pool (for example
/// the pipeline's shutdown watcher) can open the window.
#[derive(Clone)]
pub struct ShutdownRetryGrace {
    inner: Arc<RetryGraceInner>,
}

struct RetryGraceInner {
    /// Set once the window has opened; later `begin` calls are no-ops.
    is_started: std::sync::atomic::AtomicBool,
    /// Window length in milliseconds.
    grace_ms: std::sync::atomic::AtomicU64,
    /// Fired when the window closes. Workers watch this token.
    stop: CancellationToken,
}

impl ShutdownRetryGrace {
    /// Create a window of length `grace` that has not opened yet.
    pub fn new(grace: Duration) -> Self {
        ShutdownRetryGrace {
            inner: Arc::new(RetryGraceInner {
                is_started: std::sync::atomic::AtomicBool::new(false),
                grace_ms: std::sync::atomic::AtomicU64::new(duration_to_millis(grace)),
                stop: CancellationToken::new(),
            }),
        }
    }

    /// Change the window length. Has no effect once the window has opened.
    pub fn set_grace(&self, grace: Duration) {
        self.inner
            .grace_ms
            .store(duration_to_millis(grace), std::sync::atomic::Ordering::Release);
    }

    /// The configured window length.
    pub fn grace(&self) -> Duration {
        Duration::from_millis(
            self.inner
                .grace_ms
                .load(std::sync::atomic::Ordering::Acquire),
        )
    }

    /// Open the window: shutdown has begun.
    ///
    /// Idempotent. A zero-length window closes immediately, so failed sends
    /// are not retried at all. Otherwise a timer task closes it after the
    /// configured grace. Must be called from within a Tokio runtime.
    pub fn begin(&self) {
        self.open(None);
    }

    /// Like [`Self::begin`], but the window is no longer than `cap`.
    ///
    /// The pool drain passes its own timeout here so the retry window always
    /// closes before the drain gives up and cancels workers.
    pub fn begin_capped(&self, cap: Duration) {
        self.open(Some(cap));
    }

    fn open(&self, cap: Option<Duration>) {
        if self
            .inner
            .is_started
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let configured = self.grace();
        let grace = cap.map_or(configured, |cap| configured.min(cap));
        if grace.is_zero() {
            self.inner.stop.cancel();
            return;
        }
        tracing::info!(
            grace_ms = duration_to_millis(grace),
            "worker_pool: shutdown started; retrying failed sends for the grace window"
        );
        let stop = self.inner.stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(grace) => {}
                () = stop.cancelled() => {}
            }
            stop.cancel();
        });
    }

    /// Close the window now, whether or not it has opened.
    pub fn expire(&self) {
        self.inner
            .is_started
            .store(true, std::sync::atomic::Ordering::Release);
        self.inner.stop.cancel();
    }

    /// Whether the window has opened.
    pub fn is_started(&self) -> bool {
        self.inner
            .is_started
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Token that fires when the window closes. Workers stop retrying then.
    pub(crate) fn stop_token(&self) -> CancellationToken {
        self.inner.stop.clone()
    }
}

fn duration_to_millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}
