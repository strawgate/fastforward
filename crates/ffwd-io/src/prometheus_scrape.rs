//! Prometheus scrape input: pulls text-format `/metrics` endpoints on an
//! interval and emits one Arrow `RecordBatch` per successful scrape.
//!
//! Each target runs as its own task on a dedicated single-threaded tokio
//! runtime. A scrape fetches the body (uncompressed by default), parses it
//! with [`ffwd_arrow::prom::PromBatchBuilder`] (SIMD scan into zero-copy
//! `Utf8View` columns), then adds the `job` and `instance` labels, fills
//! missing timestamps with the scrape time and, optionally, the
//! `__series_id__` column.
//!
//! A malformed exposition rejects the whole scrape, as Prometheus does. The
//! failure is counted, logged (rate-limited per target) and reflected in the
//! input's health; the next scrape is attempted on schedule.
//!
//! Counters, histograms and summaries get a `__start_timestamp_ms__` column
//! (see the `start_time` module). Bodies being read and batches not yet taken by the
//! pipeline share one byte budget per input (see the `budget` module): when it is
//! used up, scrapes wait instead of buffering more.

mod budget;
mod start_time;

pub use start_time::DEFAULT_MAX_TRACKED_SERIES;

use std::io;
use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arrow::array::{Array, ArrayRef, AsArray, Int64Array, StringViewArray, UInt32Array};
use arrow::datatypes::{DataType, Field, FieldRef, Int64Type, Schema};
use arrow::record_batch::{RecordBatch, RecordBatchOptions};
use bytes::{Bytes, BytesMut};
use ffwd_arrow::prom::{PromBatchBuilder, PromToArrowError, append_series_id};
use ffwd_core::prom_text::{PROM_TIMESTAMP_FIELD, PromScanConfig};
use ffwd_types::diagnostics::{ComponentHealth, ComponentStats};
use tokio::sync::{mpsc, watch};

use self::budget::{Budget, MIN_RESERVATION_BYTES, OverBudget, Reservation};
use self::start_time::StartTimeTracker;
use crate::input::{InputSource, SourceEvent};

/// Default scrape interval (Prometheus default).
pub const DEFAULT_SCRAPE_INTERVAL: Duration = Duration::from_secs(15);
/// Default per-scrape timeout (Prometheus default).
pub const DEFAULT_SCRAPE_TIMEOUT: Duration = Duration::from_secs(10);
/// Default response size limit.
pub const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Default budget for bodies being read plus batches not yet taken by the
/// pipeline, shared by every target of one input.
pub const DEFAULT_MAX_INFLIGHT_BYTES: usize = 1024 * 1024 * 1024;
/// Default limit on distinct label columns in one scrape.
pub const DEFAULT_MAX_LABEL_COLUMNS: usize = ffwd_arrow::prom::DEFAULT_MAX_LABEL_COLUMNS;

/// Batches buffered between the scrape tasks and the pipeline. A full
/// channel delays the next scrape (backpressure) rather than dropping data.
const CHANNEL_CAPACITY: usize = 64;
/// Upper bound on batches handed to the pipeline per `poll()`.
const MAX_BATCHES_PER_POLL: usize = 64;
/// Minimum gap between repeated failure logs for one target.
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(60);

const ACCEPT_HEADER: &str =
    "text/plain;version=0.0.4;q=1,application/openmetrics-text;version=1.0.0;q=0.5,*/*;q=0.1";

/// Runtime settings for [`PrometheusScrapeInput`].
#[derive(Clone, Debug)]
pub struct PrometheusScrapeSettings {
    /// Target URLs.
    pub targets: Vec<String>,
    /// `job` label value.
    pub job: String,
    /// Scrape interval per target.
    pub interval: Duration,
    /// Timeout for one scrape (connect through end of body).
    pub timeout: Duration,
    /// Request gzip instead of an uncompressed body.
    pub gzip: bool,
    /// Keep exposition timestamps; when false every sample gets the scrape time.
    pub honor_timestamps: bool,
    /// Extra request headers.
    pub headers: Vec<(String, String)>,
    /// Largest accepted body in bytes.
    pub max_body_bytes: usize,
    /// Largest number of label columns in one scrape.
    pub max_label_columns: usize,
    /// Append the `__series_id__` column.
    pub series_id: bool,
    /// Byte budget shared by all targets: bodies being read plus batches
    /// not yet taken by the pipeline.
    pub max_inflight_bytes: usize,
    /// Most series per target whose start time is remembered.
    pub max_tracked_series: usize,
}

impl PrometheusScrapeSettings {
    /// Settings with Prometheus defaults for the given targets and job.
    pub fn new(targets: Vec<String>, job: String) -> Self {
        Self {
            targets,
            job,
            interval: DEFAULT_SCRAPE_INTERVAL,
            timeout: DEFAULT_SCRAPE_TIMEOUT,
            gzip: false,
            honor_timestamps: true,
            headers: Vec::new(),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_label_columns: DEFAULT_MAX_LABEL_COLUMNS,
            series_id: true,
            max_inflight_bytes: DEFAULT_MAX_INFLIGHT_BYTES,
            max_tracked_series: DEFAULT_MAX_TRACKED_SERIES,
        }
    }
}

struct Scraped {
    batch: RecordBatch,
    body_bytes: u64,
    /// Share of the input's budget, returned once the pipeline takes the batch.
    reservation: Reservation,
}

/// Shared state between the input handle and the scrape tasks.
struct Shared {
    stats: Arc<ComponentStats>,
    /// Number of targets whose most recent scrape failed.
    failing: AtomicUsize,
    running: AtomicBool,
    budget: Arc<Budget>,
}

/// Input that scrapes Prometheus endpoints. See the module docs.
pub struct PrometheusScrapeInput {
    name: String,
    rx: mpsc::Receiver<Scraped>,
    shutdown: Option<watch::Sender<bool>>,
    handle: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    health: Arc<AtomicU8>,
}

impl PrometheusScrapeInput {
    /// Start scraping. Each target is first scraped at a stable offset within
    /// the interval (derived from its URL) so targets do not all fire at once.
    pub fn new(
        name: impl Into<String>,
        settings: PrometheusScrapeSettings,
        stats: Arc<ComponentStats>,
    ) -> io::Result<Self> {
        let name = name.into();
        if settings.targets.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prometheus_scrape requires at least one target",
            ));
        }
        if settings.interval.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prometheus_scrape interval must be positive",
            ));
        }
        let mut targets = Vec::with_capacity(settings.targets.len());
        for (i, url) in settings.targets.iter().enumerate() {
            // Trimmed the same way config validation trims it.
            let url = url.trim();
            // Errors name the target by index: the URL may carry credentials.
            let parsed = reqwest::Url::parse(url).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("targets[{i}]: {e}"))
            })?;
            let host = parsed.host_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("targets[{i}] has no host"),
                )
            })?;
            let instance = match parsed.port_or_known_default() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_string(),
            };
            targets.push(Target {
                display: redact_url(url),
                url: parsed,
                instance,
            });
        }

        let client = build_client(&settings).map_err(io::Error::other)?;

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            stats,
            failing: AtomicUsize::new(0),
            running: AtomicBool::new(true),
            budget: Arc::new(Budget::new(settings.max_inflight_bytes)),
        });
        let health = Arc::new(AtomicU8::new(ComponentHealth::Starting.as_repr()));
        let settings = Arc::new(settings);

        let task_shared = Arc::clone(&shared);
        let task_health = Arc::clone(&health);
        let task_name = name.clone();
        let handle = std::thread::Builder::new()
            .name("prom-scrape".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!(input = %task_name, error = %e, "prometheus_scrape: failed to start runtime");
                        task_health.store(ComponentHealth::Failed.as_repr(), Ordering::Relaxed);
                        return;
                    }
                };
                task_health.store(ComponentHealth::Healthy.as_repr(), Ordering::Relaxed);
                runtime.block_on(async move {
                    let mut set = tokio::task::JoinSet::new();
                    for target in targets {
                        set.spawn(run_target(
                            task_name.clone(),
                            target,
                            client.clone(),
                            Arc::clone(&settings),
                            Arc::clone(&task_shared),
                            tx.clone(),
                            shutdown_rx.clone(),
                        ));
                    }
                    drop(tx);
                    while set.join_next().await.is_some() {}
                });
            })
            .map_err(io::Error::other)?;

        Ok(Self {
            name,
            rx,
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
            shared,
            health,
        })
    }
}

impl Drop for PrometheusScrapeInput {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::Relaxed);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(true);
        }
        // Unblock tasks waiting on a full channel.
        self.rx.close();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl InputSource for PrometheusScrapeInput {
    fn poll(&mut self) -> io::Result<Vec<SourceEvent>> {
        let mut events = Vec::new();
        while events.len() < MAX_BATCHES_PER_POLL {
            match self.rx.try_recv() {
                Ok(s) => {
                    // The pipeline owns the batch from here.
                    drop(s.reservation);
                    events.push(SourceEvent::Batch {
                        batch: s.batch,
                        source_id: None,
                        accounted_bytes: s.body_bytes,
                    });
                }
                Err(_) => break,
            }
        }
        Ok(events)
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn health(&self) -> ComponentHealth {
        let stored = ComponentHealth::from_repr(self.health.load(Ordering::Relaxed));
        let thread_died = self.handle.as_ref().is_some_and(JoinHandle::is_finished)
            && self.shared.running.load(Ordering::Relaxed);
        if thread_died {
            return ComponentHealth::Failed;
        }
        if stored == ComponentHealth::Healthy && self.shared.failing.load(Ordering::Relaxed) > 0 {
            return ComponentHealth::Degraded;
        }
        stored
    }
}

struct Target {
    url: reqwest::Url,
    /// `url` without credentials or query string, for logs.
    display: String,
    instance: String,
}

/// A target URL safe to log: scheme, host and port only. User info, path,
/// query string and fragment are dropped since any of them can carry
/// secrets (some exporters take a token in the path).
/// Unparsable input is replaced by a placeholder.
pub fn redact_url(url: &str) -> String {
    match reqwest::Url::parse(url.trim()) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("");
            match u.port() {
                Some(port) => format!("{}://{host}:{port}", u.scheme()),
                None => format!("{}://{host}", u.scheme()),
            }
        }
        Err(_) => "<invalid url>".to_string(),
    }
}

fn is_same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Most redirects followed for one scrape (reqwest's default).
const MAX_REDIRECTS: usize = 10;

/// HTTP client for all targets of one input.
///
/// When custom headers are configured (often credentials), redirects are
/// followed only within the original scheme, host and port so the headers
/// never reach another server; a cross-origin redirect fails the scrape.
fn build_client(settings: &PrometheusScrapeSettings) -> reqwest::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .no_gzip()
        .user_agent(concat!("ffwd/", env!("CARGO_PKG_VERSION")));
    if !settings.headers.is_empty() {
        builder = builder.redirect(reqwest::redirect::Policy::custom(|attempt| {
            let previous = attempt.previous();
            if previous.len() > MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            let same = previous
                .first()
                .is_some_and(|first| is_same_origin(first, attempt.url()));
            if same {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }));
    }
    builder.build()
}

/// Stable start offset in `[0, interval)` for a target, so a fleet of
/// targets spreads over the interval instead of firing together.
fn start_offset(url: &str, interval: Duration) -> Duration {
    let h = xxhash_rust::xxh64::xxh64(url.as_bytes(), 0);
    let nanos = interval.as_nanos().max(1);
    let off = u128::from(h) % nanos;
    Duration::from_nanos(u64::try_from(off).unwrap_or(0))
}

#[allow(clippy::too_many_arguments)]
async fn run_target(
    input: String,
    target: Target,
    client: reqwest::Client,
    settings: Arc<PrometheusScrapeSettings>,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Scraped>,
    mut shutdown: watch::Receiver<bool>,
) {
    let offset = start_offset(target.url.as_str(), settings.interval);
    tokio::select! {
        () = tokio::time::sleep(offset) => {}
        _ = shutdown.changed() => return,
    }
    let mut ticker = tokio::time::interval(settings.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut builder = PromBatchBuilder::new(PromScanConfig::default())
        .with_max_label_columns(settings.max_label_columns);
    let mut tracker = StartTimeTracker::new(settings.max_tracked_series);
    // Budget reserved before each request: what the last scrape needed.
    let mut estimate = MIN_RESERVATION_BYTES;
    let mut failing = false;
    let mut last_error_log: Option<Instant> = None;
    let mut suppressed = 0u64;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.changed() => return,
        }
        // A scrape can take up to the timeout; don't hold up shutdown for it.
        let result = tokio::select! {
            r = scrape_once(&client, &target, &settings, &shared, &mut estimate, &mut builder, &mut tracker) => r,
            _ = shutdown.changed() => return,
        };
        match result {
            Ok(scraped) => {
                if failing {
                    failing = false;
                    shared.failing.fetch_sub(1, Ordering::Relaxed);
                    tracing::info!(input = %input, target = %target.display, "prometheus_scrape: target recovered");
                }
                let sent = tokio::select! {
                    r = tx.send(scraped) => r.is_ok(),
                    _ = shutdown.changed() => false,
                };
                if !sent {
                    return;
                }
            }
            Err(e) => {
                shared.stats.inc_errors();
                if !failing {
                    failing = true;
                    shared.failing.fetch_add(1, Ordering::Relaxed);
                }
                let now = Instant::now();
                if last_error_log.is_none_or(|t| now.duration_since(t) >= ERROR_LOG_INTERVAL) {
                    tracing::warn!(
                        input = %input,
                        target = %target.display,
                        error = %e,
                        suppressed,
                        "prometheus_scrape: scrape failed"
                    );
                    last_error_log = Some(now);
                    suppressed = 0;
                } else {
                    suppressed += 1;
                }
            }
        }
    }
}

/// Why one scrape failed.
#[derive(Debug)]
enum ScrapeError {
    Http(reqwest::Error),
    Status(u16),
    TooLarge(usize),
    Gzip(io::Error),
    Parse(PromToArrowError),
    Arrow(arrow::error::ArrowError),
    /// The input's `max_inflight_bytes` had no room before the timeout.
    OverBudget(usize),
}

impl std::fmt::Display for ScrapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "request failed: {e}"),
            Self::Status(s) => write!(f, "server returned HTTP {s}"),
            Self::TooLarge(limit) => write!(f, "body exceeds max_body_bytes ({limit})"),
            Self::Gzip(e) => write!(f, "gzip decode failed: {e}"),
            Self::Parse(e) => write!(f, "{e}"),
            Self::Arrow(e) => write!(f, "arrow: {e}"),
            Self::OverBudget(n) => write!(
                f,
                "no room in max_inflight_bytes for {n} bytes before the timeout; the next scrape reserves it first"
            ),
        }
    }
}

/// reqwest errors embed the request URL, which may carry credentials.
fn http_error(e: reqwest::Error) -> ScrapeError {
    ScrapeError::Http(e.without_url())
}

/// True for `application/openmetrics-text` (any parameters).
fn is_openmetrics_content_type(value: &[u8]) -> bool {
    let media = value.split(|&b| b == b';').next().unwrap_or_default();
    media
        .trim_ascii()
        .eq_ignore_ascii_case(b"application/openmetrics-text")
}

#[allow(clippy::too_many_arguments)]
async fn scrape_once(
    client: &reqwest::Client,
    target: &Target,
    settings: &PrometheusScrapeSettings,
    shared: &Shared,
    estimate: &mut usize,
    builder: &mut PromBatchBuilder,
    tracker: &mut StartTimeTracker,
) -> Result<Scraped, ScrapeError> {
    let (mut reservation, waited) = shared
        .budget
        .reserve(*estimate)
        .await
        .map_err(|e| ScrapeError::OverBudget(e.needed))?;
    if waited {
        tracing::debug!(
            target = %target.display,
            "prometheus_scrape: scrape delayed until max_inflight_bytes had room"
        );
    }
    // Growth past the estimate may wait only until the scrape's timeout.
    let deadline = tokio::time::Instant::now() + settings.timeout;
    let mut peak = 0usize;
    let result = scrape_reserved(
        client,
        target,
        settings,
        builder,
        tracker,
        &mut reservation,
        deadline,
        &mut peak,
    )
    .await;
    // The next scrape reserves what this one needed, plus headroom.
    *estimate = peak.saturating_add(peak / 4).max(MIN_RESERVATION_BYTES);
    let (batch, body_bytes) = result?;
    reservation.shrink_to(batch.get_array_memory_size());
    Ok(Scraped {
        batch,
        body_bytes,
        reservation,
    })
}

/// Grow `reservation` to cover `bytes` and record the peak.
async fn hold(
    reservation: &mut Reservation,
    bytes: usize,
    deadline: tokio::time::Instant,
    peak: &mut usize,
) -> Result<(), ScrapeError> {
    *peak = (*peak).max(bytes);
    reservation
        .grow_to(bytes, deadline)
        .await
        .map_err(|OverBudget { needed }| ScrapeError::OverBudget(needed))
}

#[allow(clippy::too_many_arguments)]
async fn scrape_reserved(
    client: &reqwest::Client,
    target: &Target,
    settings: &PrometheusScrapeSettings,
    builder: &mut PromBatchBuilder,
    tracker: &mut StartTimeTracker,
    reservation: &mut Reservation,
    deadline: tokio::time::Instant,
    peak: &mut usize,
) -> Result<(RecordBatch, u64), ScrapeError> {
    let scrape_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let mut req = client
        .get(target.url.clone())
        .timeout(settings.timeout)
        .header(reqwest::header::ACCEPT, ACCEPT_HEADER)
        .header(
            reqwest::header::ACCEPT_ENCODING,
            if settings.gzip { "gzip" } else { "identity" },
        )
        .header(
            "X-Prometheus-Scrape-Timeout-Seconds",
            format!("{}", settings.timeout.as_secs_f64()),
        );
    for (k, v) in &settings.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let mut resp = req.send().await.map_err(http_error)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ScrapeError::Status(status.as_u16()));
    }
    let limit = settings.max_body_bytes;
    if resp.content_length().is_some_and(|n| n > limit as u64) {
        return Err(ScrapeError::TooLarge(limit));
    }
    let resp_is_openmetrics = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .is_some_and(|v| is_openmetrics_content_type(v.as_bytes()));
    let is_gzip = resp
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"gzip"));
    let initial = resp.content_length().map_or(64 * 1024, |n| {
        usize::try_from(n).unwrap_or(limit).min(limit)
    });
    hold(reservation, initial, deadline, peak).await?;
    let mut buf = BytesMut::with_capacity(initial);
    while let Some(chunk) = resp.chunk().await.map_err(http_error)? {
        if buf.len() + chunk.len() > limit {
            return Err(ScrapeError::TooLarge(limit));
        }
        buf.extend_from_slice(&chunk);
        if buf.capacity() > reservation.bytes() {
            hold(reservation, buf.capacity(), deadline, peak).await?;
        }
    }
    let wire_bytes = buf.len() as u64;
    // OpenMetrics timestamps are float seconds; the text format uses
    // integer milliseconds.
    builder.config_mut().openmetrics = resp_is_openmetrics;
    let body: Bytes = if is_gzip {
        let decoded = gunzip_limited(&buf, limit)?;
        hold(reservation, buf.capacity() + decoded.len(), deadline, peak).await?;
        decoded
    } else {
        buf.freeze()
    };

    let (batch, _stats) = builder.scan(body).map_err(ScrapeError::Parse)?;
    let batch = finish_batch(
        &batch,
        &settings.job,
        &target.instance,
        scrape_ms,
        settings.honor_timestamps,
    )
    .map_err(ScrapeError::Arrow)?;
    let batch = tracker.apply(&batch).map_err(ScrapeError::Arrow)?;
    let batch = if settings.series_id {
        append_series_id(&batch).map_err(ScrapeError::Arrow)?
    } else {
        batch
    };
    hold(reservation, batch.get_array_memory_size(), deadline, peak).await?;
    Ok((batch, wire_bytes))
}

fn gunzip_limited(input: &[u8], limit: usize) -> Result<Bytes, ScrapeError> {
    let mut out = Vec::with_capacity(input.len().saturating_mul(4).min(limit));
    // Multi-member: concatenated gzip streams are valid and decode in full.
    let decoder = flate2::read::MultiGzDecoder::new(input);
    let read = decoder
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut out)
        .map_err(ScrapeError::Gzip)?;
    if read > limit {
        return Err(ScrapeError::TooLarge(limit));
    }
    Ok(Bytes::from(out))
}

/// Add target labels and fill timestamps, following Prometheus semantics:
///
/// - Exposition labels named `job` / `instance` are renamed to
///   `exported_job` / `exported_instance` (Prometheus `honor_labels: false`).
///   If a sample already has that label, `exported_` is prepended again
///   until the name is free for that sample, as Prometheus does. This is
///   decided per sample, so rows without `exported_job` still use
///   `exported_job`.
/// - `job` and `instance` columns are appended for every row.
/// - Samples without a timestamp get the scrape time; with
///   `honor_timestamps = false`, every sample does.
pub fn finish_batch(
    batch: &RecordBatch,
    job: &str,
    instance: &str,
    scrape_ms: i64,
    honor_timestamps: bool,
) -> Result<RecordBatch, arrow::error::ArrowError> {
    let num_rows = batch.num_rows();
    let schema = batch.schema();
    let mut fields: Vec<FieldRef> = Vec::with_capacity(schema.fields().len() + 2);
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len() + 2);
    let mut has_ts = false;
    let mut replaced: Vec<(usize, ArrayRef)> = Vec::new();
    let mut appended: Vec<(FieldRef, ArrayRef)> = Vec::new();
    for label in ["job", "instance"] {
        if let Some(plan) = plan_export(batch, label)? {
            replaced.extend(plan.replaced);
            appended.extend(plan.appended);
        }
    }
    for (i, (field, col)) in schema.fields().iter().zip(batch.columns()).enumerate() {
        let name = field.name().as_str();
        if let Some((_, merged)) = replaced.iter().find(|(idx, _)| *idx == i) {
            fields.push(Arc::new(Field::new(name, DataType::Utf8View, true)));
            arrays.push(Arc::clone(merged));
            continue;
        }
        match name {
            // Moved to an `exported_` column by `plan_export`.
            "job" | "instance" => {}
            _ if name.as_bytes() == PROM_TIMESTAMP_FIELD => {
                has_ts = true;
                let ts = col.as_primitive::<Int64Type>();
                let filled: Int64Array = if honor_timestamps {
                    if ts.null_count() == 0 {
                        ts.clone()
                    } else {
                        ts.iter().map(|v| Some(v.unwrap_or(scrape_ms))).collect()
                    }
                } else {
                    Int64Array::from_value(scrape_ms, num_rows)
                };
                fields.push(Arc::new(Field::new(name, DataType::Int64, false)));
                arrays.push(Arc::new(filled));
            }
            _ => {
                fields.push(Arc::clone(field));
                arrays.push(Arc::clone(col));
            }
        }
    }
    if !has_ts {
        fields.push(Arc::new(Field::new(
            String::from_utf8_lossy(PROM_TIMESTAMP_FIELD).into_owned(),
            DataType::Int64,
            false,
        )));
        arrays.push(Arc::new(Int64Array::from_value(scrape_ms, num_rows)));
    }
    for (field, array) in appended {
        fields.push(field);
        arrays.push(array);
    }
    for (label, value) in [("job", job), ("instance", instance)] {
        fields.push(Arc::new(Field::new(label, DataType::Utf8View, false)));
        arrays.push(constant_string(value, num_rows)?);
    }
    let opts = RecordBatchOptions::new().with_row_count(Some(num_rows));
    let schema = Schema::new_with_metadata(fields, schema.metadata().clone());
    RecordBatch::try_new_with_options(Arc::new(schema), arrays, &opts)
}

/// Where the exposition's own `job` / `instance` values go.
struct ExportPlan {
    /// Existing `exported_*` columns (by index) with values merged in.
    replaced: Vec<(usize, ArrayRef)>,
    /// A new `exported_*` column, when some sample needs one.
    appended: Option<(FieldRef, ArrayRef)>,
}

/// Plan the per-sample rename of the exposition label `label`.
///
/// Each sample's value goes to the first of `exported_<label>`,
/// `exported_exported_<label>`, ... that the sample does not already have.
/// Returns `None` when the exposition has no `label` column.
fn plan_export(
    batch: &RecordBatch,
    label: &str,
) -> Result<Option<ExportPlan>, arrow::error::ArrowError> {
    let schema = batch.schema();
    let Ok(src_idx) = schema.index_of(label) else {
        return Ok(None);
    };
    // Existing chain: exported_<label>, exported_exported_<label>, ...
    let mut chain: Vec<usize> = Vec::new();
    let mut name = format!("exported_{label}");
    while let Ok(idx) = schema.index_of(&name) {
        chain.push(idx);
        name.insert_str(0, "exported_");
    }
    let src_field = schema.field(src_idx);
    if chain.is_empty() {
        // Common case: a plain rename.
        let field = Arc::new(src_field.clone().with_name(name));
        return Ok(Some(ExportPlan {
            replaced: Vec::new(),
            appended: Some((field, Arc::clone(batch.column(src_idx)))),
        }));
    }
    let as_view = |idx: usize| arrow::compute::cast(batch.column(idx), &DataType::Utf8View);
    let src = as_view(src_idx)?;
    let src = src.as_string_view();
    let chain_cols = chain
        .iter()
        .map(|&idx| as_view(idx))
        .collect::<Result<Vec<_>, _>>()?;
    let chain_cols: Vec<&StringViewArray> =
        chain_cols.iter().map(AsArray::as_string_view).collect();
    // Slot each row's value lands in: 0..chain.len() is an existing column,
    // chain.len() is the new one.
    let slots: Vec<Option<usize>> = (0..batch.num_rows())
        .map(|row| {
            src.is_valid(row).then(|| {
                chain_cols
                    .iter()
                    .position(|c| c.is_null(row))
                    .unwrap_or(chain_cols.len())
            })
        })
        .collect();
    let merge = |slot: usize, existing: Option<&StringViewArray>| -> ArrayRef {
        let merged: StringViewArray = slots
            .iter()
            .enumerate()
            .map(|(row, s)| {
                if *s == Some(slot) {
                    Some(src.value(row))
                } else {
                    existing.and_then(|c| c.is_valid(row).then(|| c.value(row)))
                }
            })
            .collect();
        Arc::new(merged)
    };
    let replaced = chain
        .iter()
        .zip(&chain_cols)
        .enumerate()
        .filter(|(slot, _)| slots.contains(&Some(*slot)))
        .map(|(slot, (&idx, col))| (idx, merge(slot, Some(col))))
        .collect();
    let appended = slots.contains(&Some(chain.len())).then(|| {
        let field = Arc::new(Field::new(name, DataType::Utf8View, true));
        (field, merge(chain.len(), None))
    });
    Ok(Some(ExportPlan { replaced, appended }))
}

/// A `Utf8View` column repeating `value`; all rows share one data buffer.
fn constant_string(value: &str, num_rows: usize) -> Result<ArrayRef, arrow::error::ArrowError> {
    let one = StringViewArray::from_iter_values([value]);
    let indices = UInt32Array::from(vec![0u32; num_rows]);
    arrow::compute::take(&one, &indices, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;

    fn scan(text: &'static str) -> RecordBatch {
        PromBatchBuilder::default()
            .scan(Bytes::from_static(text.as_bytes()))
            .expect("valid exposition")
            .0
    }

    #[test]
    fn finish_batch_adds_target_labels_and_scrape_time() {
        let batch = scan("a{job=\"inner\"} 1\nb 2 1000\n");
        let out = finish_batch(&batch, "node", "h:9100", 5000, true).expect("finish");
        let job = out.column_by_name("job").expect("job").as_string_view();
        assert_eq!(job.value(0), "node");
        assert_eq!(job.value(1), "node");
        let inst = out
            .column_by_name("instance")
            .expect("instance")
            .as_string_view();
        assert_eq!(inst.value(1), "h:9100");
        let exported = out
            .column_by_name("exported_job")
            .expect("exported_job")
            .as_string_view();
        assert_eq!(exported.value(0), "inner");
        assert!(exported.is_null(1));
        let ts = out
            .column_by_name("__timestamp_ms__")
            .expect("ts")
            .as_primitive::<Int64Type>();
        assert_eq!(ts.values().as_ref(), &[5000, 1000]);
        assert_eq!(ts.null_count(), 0);
    }

    #[test]
    fn finish_batch_can_ignore_exposition_timestamps() {
        let batch = scan("a 1 1000\n");
        let out = finish_batch(&batch, "j", "i", 7, false).expect("finish");
        let ts = out
            .column_by_name("__timestamp_ms__")
            .expect("ts")
            .as_primitive::<Int64Type>();
        assert_eq!(ts.value(0), 7);
    }

    #[test]
    fn finish_batch_adds_timestamp_column_when_absent() {
        let batch = scan("a 1\n");
        let out = finish_batch(&batch, "j", "i", 42, true).expect("finish");
        let ts = out
            .column_by_name("__timestamp_ms__")
            .expect("ts")
            .as_primitive::<Int64Type>();
        assert_eq!(ts.value(0), 42);
    }

    #[test]
    fn start_offsets_are_stable_and_within_interval() {
        let interval = Duration::from_secs(15);
        let a = start_offset("http://a:9100/metrics", interval);
        assert_eq!(a, start_offset("http://a:9100/metrics", interval));
        assert!(a < interval);
        assert_ne!(a, start_offset("http://b:9100/metrics", interval));
    }

    #[test]
    fn gunzip_respects_limit() {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&[b'a'; 1000]).expect("write");
        let gz = enc.finish().expect("finish");
        assert_eq!(gunzip_limited(&gz, 1000).expect("fits").len(), 1000);
        assert!(matches!(
            gunzip_limited(&gz, 999),
            Err(ScrapeError::TooLarge(999))
        ));
    }

    /// Serve `body` to every connection with the given extra header lines.
    fn serve(body: Vec<u8>, extra_headers: &'static str) -> String {
        serve_typed(body, "text/plain; version=0.0.4", extra_headers)
    }

    fn serve_typed(
        body: Vec<u8>,
        content_type: &'static str,
        extra_headers: &'static str,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut req = [0u8; 4096];
                let _ = stream.read(&mut req);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        format!("http://{addr}/metrics")
    }

    fn poll_until_batch(input: &mut PrometheusScrapeInput) -> RecordBatch {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let events = input.poll().expect("poll");
            if let Some(SourceEvent::Batch { batch, .. }) = events.into_iter().next() {
                return batch;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no batch within 10s");
    }

    #[test]
    fn scrapes_plain_and_gzip_targets() {
        let text = b"# TYPE up gauge\nup{zone=\"a\"} 1\n".to_vec();
        let plain = serve(text.clone(), "");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&text).expect("write");
        let gz = serve(enc.finish().expect("gz"), "Content-Encoding: gzip\r\n");

        for (url, gzip) in [(plain, false), (gz, true)] {
            let mut settings = PrometheusScrapeSettings::new(vec![url.clone()], "test".into());
            settings.interval = Duration::from_millis(50);
            settings.timeout = Duration::from_secs(5);
            settings.gzip = gzip;
            let mut input =
                PrometheusScrapeInput::new("prom", settings, Arc::new(ComponentStats::new()))
                    .expect("input");
            let batch = poll_until_batch(&mut input);
            assert_eq!(batch.num_rows(), 1, "{url}");
            let zone = batch.column_by_name("zone").expect("zone").as_string_view();
            assert_eq!(zone.value(0), "a");
            let job = batch.column_by_name("job").expect("job").as_string_view();
            assert_eq!(job.value(0), "test");
            assert!(batch.column_by_name("__series_id__").is_some());
            assert_eq!(input.health(), ComponentHealth::Healthy);
        }
    }

    #[test]
    fn openmetrics_response_timestamps_are_seconds() {
        assert!(is_openmetrics_content_type(
            b"application/openmetrics-text; version=1.0.0; charset=utf-8"
        ));
        assert!(!is_openmetrics_content_type(b"text/plain; version=0.0.4"));
        let url = serve_typed(
            b"# TYPE up gauge\nup 1 1520879607.789\n# EOF\n".to_vec(),
            "application/openmetrics-text; version=1.0.0",
            "",
        );
        let mut settings = PrometheusScrapeSettings::new(vec![url], "test".into());
        settings.interval = Duration::from_millis(50);
        let mut input =
            PrometheusScrapeInput::new("prom", settings, Arc::new(ComponentStats::new()))
                .expect("input");
        let batch = poll_until_batch(&mut input);
        let ts = batch
            .column_by_name(std::str::from_utf8(PROM_TIMESTAMP_FIELD).expect("ascii"))
            .expect("timestamp")
            .as_primitive::<Int64Type>();
        assert_eq!(ts.value(0), 1_520_879_607_789);
    }

    #[test]
    fn targets_are_trimmed_like_config_validation() {
        let url = serve_typed(b"up 1\n".to_vec(), "text/plain; version=0.0.4", "");
        let padded = format!("\u{a0} {url}\u{a0}");
        let mut settings = PrometheusScrapeSettings::new(vec![padded], "test".into());
        settings.interval = Duration::from_millis(50);
        let mut input =
            PrometheusScrapeInput::new("prom", settings, Arc::new(ComponentStats::new()))
                .expect("padded target should be accepted");
        assert_eq!(poll_until_batch(&mut input).num_rows(), 1);
    }

    #[test]
    fn zero_interval_is_rejected() {
        let mut settings = PrometheusScrapeSettings::new(vec!["http://h/m".into()], "t".into());
        settings.interval = Duration::ZERO;
        let err = PrometheusScrapeInput::new("prom", settings, Arc::new(ComponentStats::new()))
            .err()
            .expect("zero interval");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn malformed_scrape_degrades_health_and_counts_error() {
        let url = serve(b"broken{\n".to_vec(), "");
        let mut settings = PrometheusScrapeSettings::new(vec![url], "test".into());
        settings.interval = Duration::from_millis(20);
        assert!(
            matches!(scrape_direct(&settings), Err(ScrapeError::Parse(_))),
            "a malformed body is a parse error, not a transport error"
        );
        let stats = Arc::new(ComponentStats::new());
        let input =
            PrometheusScrapeInput::new("prom", settings, Arc::clone(&stats)).expect("input");
        let deadline = Instant::now() + Duration::from_secs(10);
        while input.health() != ComponentHealth::Degraded {
            assert!(Instant::now() < deadline, "health never degraded");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(stats.errors() > 0);
    }

    /// Run one scrape of `settings.targets[0]` on a fresh runtime.
    fn scrape_direct(settings: &PrometheusScrapeSettings) -> Result<Scraped, ScrapeError> {
        let client = build_client(settings).expect("client");
        let raw = &settings.targets[0];
        let target = Target {
            url: reqwest::Url::parse(raw).expect("url"),
            display: redact_url(raw),
            instance: "t:1".into(),
        };
        let mut builder = PromBatchBuilder::new(PromScanConfig::default())
            .with_max_label_columns(settings.max_label_columns);
        let mut tracker = StartTimeTracker::new(settings.max_tracked_series);
        let shared = Shared {
            stats: Arc::new(ComponentStats::new()),
            failing: AtomicUsize::new(0),
            running: AtomicBool::new(true),
            budget: Arc::new(Budget::new(settings.max_inflight_bytes)),
        };
        let mut estimate = MIN_RESERVATION_BYTES;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(scrape_once(
                &client,
                &target,
                settings,
                &shared,
                &mut estimate,
                &mut builder,
                &mut tracker,
            ))
    }

    #[test]
    fn scrapes_wait_for_budget_while_the_pipeline_is_blocked() {
        let body = b"# TYPE reqs counter\nreqs_total 1\n".to_vec();
        let targets: Vec<String> = (0..8).map(|_| serve(body.clone(), "")).collect();
        let mut settings = PrometheusScrapeSettings::new(targets, "test".into());
        settings.interval = Duration::from_millis(20);
        settings.timeout = Duration::from_secs(5);
        // Smaller than one scrape: each scrape takes the whole budget, so
        // only one batch can be waiting at a time.
        settings.max_inflight_bytes = 1;
        let mut input =
            PrometheusScrapeInput::new("prom", settings, Arc::new(ComponentStats::new()))
                .expect("input");
        // The pipeline does not poll: eight targets, many intervals.
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(input.rx.len(), 1, "budget bounds queued batches");
        assert_eq!(input.shared.budget.available_bytes(), 0);
        // Draining resumes scraping.
        let mut seen = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        while seen < 4 && Instant::now() < deadline {
            seen += input.poll().expect("poll").len();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            seen >= 4,
            "scrapes resume after the pipeline drains ({seen})"
        );
    }

    #[test]
    fn scraped_counters_carry_start_times_from_created_samples() {
        let body =
            b"# TYPE jobs counter\njobs_total 3\njobs_created 1700000000.5\n# EOF\n".to_vec();
        let url = serve_typed(body, "application/openmetrics-text; version=1.0.0", "");
        let settings = PrometheusScrapeSettings::new(vec![url], "test".into());
        let batch = scrape_direct(&settings).expect("scrape").batch;
        let starts = batch
            .column_by_name(ffwd_arrow::prom::PROM_START_TIMESTAMP_FIELD)
            .expect("start column")
            .as_primitive::<Int64Type>();
        assert_eq!(starts.value(0), 1_700_000_000_500);
        assert!(starts.is_null(1), "the _created sample itself has none");
    }

    /// Serve a redirect from `/metrics` to `location` (a path or URL), and
    /// `body` on any other path.
    fn serve_redirect(location: Option<String>, body: &'static [u8]) -> String {
        serve_redirect_requiring(location, body, None)
    }

    /// Like [`serve_redirect`], but any other path answers 401 unless the
    /// request carries the header line `required` (matched case-insensitively).
    fn serve_redirect_requiring(
        location: Option<String>,
        body: &'static [u8],
        required: Option<&'static str>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                // Read the whole request head (bounded, with a timeout): one
                // read may return before the headers we check arrive.
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut req = Vec::with_capacity(1024);
                let mut chunk = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") && req.len() < 16 * 1024 {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&chunk[..n]),
                    }
                }
                let is_metrics = req.starts_with(b"GET /metrics ");
                let request = String::from_utf8_lossy(&req).to_ascii_lowercase();
                let is_authorized =
                    required.is_none_or(|h| request.contains(&h.to_ascii_lowercase()));
                let head = match (&location, is_metrics) {
                    (Some(loc), true) => format!(
                        "HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                    _ if !is_authorized => {
                        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    }
                    _ => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    ),
                };
                let _ = stream.write_all(head.as_bytes());
                if head.starts_with("HTTP/1.1 200") {
                    let _ = stream.write_all(body);
                }
            }
        });
        format!("http://{addr}/metrics")
    }

    #[test]
    fn rejects_scrape_with_too_many_label_columns() {
        let url = serve(b"m{a=\"1\",b=\"2\",c=\"3\"} 1\n".to_vec(), "");
        let mut settings = PrometheusScrapeSettings::new(vec![url], "test".into());
        settings.max_label_columns = 2;
        assert!(matches!(
            scrape_direct(&settings),
            Err(ScrapeError::Parse(PromToArrowError::TooManyLabelColumns {
                limit: 2
            }))
        ));
    }

    #[test]
    fn headers_are_not_sent_across_origins_on_redirect() {
        let other = serve_redirect(None, b"up 1\n");
        let url = serve_redirect(Some(other), b"");
        let mut settings = PrometheusScrapeSettings::new(vec![url.clone()], "test".into());
        settings.headers = vec![("Authorization".into(), "Bearer secret".into())];
        assert!(matches!(
            scrape_direct(&settings),
            Err(ScrapeError::Status(302))
        ));
        // Without custom headers the redirect is followed.
        settings.headers.clear();
        let scraped = scrape_direct(&settings).expect("followed");
        assert_eq!(scraped.batch.num_rows(), 1);
    }

    #[test]
    fn same_origin_redirect_is_followed_with_headers() {
        // `/real` answers 401 unless the header survived the redirect.
        let url = serve_redirect_requiring(
            Some("/real".into()),
            b"up 1\n",
            Some("authorization: Bearer secret"),
        );
        let mut settings = PrometheusScrapeSettings::new(vec![url], "test".into());
        settings.headers = vec![("Authorization".into(), "Bearer secret".into())];
        let scraped = scrape_direct(&settings).expect("followed with the header");
        assert_eq!(scraped.batch.num_rows(), 1);
        settings.headers.clear();
        assert!(matches!(
            scrape_direct(&settings),
            Err(ScrapeError::Status(401))
        ));
    }

    #[test]
    fn urls_are_redacted_for_logs() {
        assert_eq!(
            redact_url("https://user:pw@h:9100/metrics?token=x#f"),
            "https://h:9100"
        );
        assert_eq!(redact_url("http://h/metrics/s3cr3t"), "http://h");
        assert_eq!(redact_url("  http://h/metrics "), "http://h");
        assert_eq!(redact_url("not a url"), "<invalid url>");
    }

    #[test]
    fn gunzip_reads_every_member() {
        let mut body = Vec::new();
        for part in [&b"a 1\n"[..], b"b 2\n"] {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(part).expect("write");
            body.extend(enc.finish().expect("finish"));
        }
        assert_eq!(
            gunzip_limited(&body, 1024).expect("gunzip").as_ref(),
            b"a 1\nb 2\n"
        );
    }

    #[test]
    fn exported_label_names_do_not_collide() {
        let batch = scan("a{job=\"x\",exported_job=\"y\"} 1\n");
        let out = finish_batch(&batch, "node", "h:1", 0, true).expect("finish");
        let col = |n: &str| {
            out.column_by_name(n)
                .unwrap_or_else(|| panic!("{n}"))
                .as_string_view()
                .value(0)
                .to_string()
        };
        assert_eq!(col("job"), "node");
        assert_eq!(col("exported_job"), "y");
        assert_eq!(col("exported_exported_job"), "x");
    }

    #[test]
    fn exported_label_names_are_chosen_per_sample() {
        let batch = scan("a{job=\"x\"} 1\nb{job=\"x\",exported_job=\"y\"} 2\nc 3\n");
        let out = finish_batch(&batch, "node", "h:1", 0, true).expect("finish");
        let col = |n: &str| -> Vec<Option<String>> {
            let c = out.column_by_name(n).unwrap_or_else(|| panic!("{n}"));
            let c = c.as_string_view();
            (0..c.len())
                .map(|i| c.is_valid(i).then(|| c.value(i).to_string()))
                .collect()
        };
        let s = |v: &str| Some(v.to_string());
        assert_eq!(col("exported_job"), vec![s("x"), s("y"), None]);
        assert_eq!(col("exported_exported_job"), vec![None, s("x"), None]);
        assert_eq!(col("job"), vec![s("node"), s("node"), s("node")]);
    }
}
