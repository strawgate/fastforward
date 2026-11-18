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

use crate::input::{InputSource, SourceEvent};

/// Default scrape interval (Prometheus default).
pub const DEFAULT_SCRAPE_INTERVAL: Duration = Duration::from_secs(15);
/// Default per-scrape timeout (Prometheus default).
pub const DEFAULT_SCRAPE_TIMEOUT: Duration = Duration::from_secs(10);
/// Default response size limit.
pub const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
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
        }
    }
}

struct Scraped {
    batch: RecordBatch,
    body_bytes: u64,
}

/// Shared state between the input handle and the scrape tasks.
struct Shared {
    stats: Arc<ComponentStats>,
    /// Number of targets whose most recent scrape failed.
    failing: AtomicUsize,
    running: AtomicBool,
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
        let mut targets = Vec::with_capacity(settings.targets.len());
        for (i, url) in settings.targets.iter().enumerate() {
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
                Ok(s) => events.push(SourceEvent::Batch {
                    batch: s.batch,
                    source_id: None,
                    accounted_bytes: s.body_bytes,
                }),
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

/// A target URL safe to log: scheme, host, port and path only. User info,
/// query string and fragment are dropped since they can carry secrets.
/// Unparseable input is replaced by a placeholder.
pub fn redact_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("");
            match u.port() {
                Some(port) => format!("{}://{host}:{port}{}", u.scheme(), u.path()),
                None => format!("{}://{host}{}", u.scheme(), u.path()),
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
            r = scrape_once(&client, &target, &settings, &mut builder) => r,
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

async fn scrape_once(
    client: &reqwest::Client,
    target: &Target,
    settings: &PrometheusScrapeSettings,
    builder: &mut PromBatchBuilder,
) -> Result<Scraped, ScrapeError> {
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
    let mut buf = BytesMut::with_capacity(resp.content_length().map_or(64 * 1024, |n| {
        usize::try_from(n).unwrap_or(limit).min(limit)
    }));
    while let Some(chunk) = resp.chunk().await.map_err(http_error)? {
        if buf.len() + chunk.len() > limit {
            return Err(ScrapeError::TooLarge(limit));
        }
        buf.extend_from_slice(&chunk);
    }
    let wire_bytes = buf.len() as u64;
    // OpenMetrics timestamps are float seconds; the text format uses
    // integer milliseconds.
    builder.config_mut().openmetrics = resp_is_openmetrics;
    let body: Bytes = if is_gzip {
        gunzip_limited(&buf, limit)?
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
    let batch = if settings.series_id {
        append_series_id(&batch).map_err(ScrapeError::Arrow)?
    } else {
        batch
    };
    Ok(Scraped {
        batch,
        body_bytes: wire_bytes,
    })
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
///   If that name is taken too, `exported_` is prepended again until it is
///   free, as Prometheus does.
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
    for (field, col) in schema.fields().iter().zip(batch.columns()) {
        let name = field.name().as_str();
        match name {
            "job" | "instance" => {
                let mut renamed = format!("exported_{name}");
                while schema.field_with_name(&renamed).is_ok() {
                    renamed.insert_str(0, "exported_");
                }
                fields.push(Arc::new(field.as_ref().clone().with_name(renamed)));
                arrays.push(Arc::clone(col));
            }
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
    for (label, value) in [("job", job), ("instance", instance)] {
        fields.push(Arc::new(Field::new(label, DataType::Utf8View, false)));
        arrays.push(constant_string(value, num_rows)?);
    }
    let opts = RecordBatchOptions::new().with_row_count(Some(num_rows));
    let schema = Schema::new_with_metadata(fields, schema.metadata().clone());
    RecordBatch::try_new_with_options(Arc::new(schema), arrays, &opts)
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
    fn malformed_scrape_degrades_health_and_counts_error() {
        let url = serve(b"broken{\n".to_vec(), "");
        let mut settings = PrometheusScrapeSettings::new(vec![url], "test".into());
        settings.interval = Duration::from_millis(20);
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
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(scrape_once(&client, &target, settings, &mut builder))
    }

    /// Serve a redirect from `/metrics` to `location` (a path or URL), and
    /// `body` on any other path.
    fn serve_redirect(location: Option<String>, body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut req = [0u8; 4096];
                let n = stream.read(&mut req).unwrap_or(0);
                let is_metrics = req[..n].starts_with(b"GET /metrics ");
                let head = match (&location, is_metrics) {
                    (Some(loc), true) => format!(
                        "HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                    _ => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    ),
                };
                let _ = stream.write_all(head.as_bytes());
                if !head.starts_with("HTTP/1.1 302") {
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
        let url = serve_redirect(Some("/real".into()), b"up 1\n");
        let mut settings = PrometheusScrapeSettings::new(vec![url], "test".into());
        settings.headers = vec![("Authorization".into(), "Bearer secret".into())];
        let scraped = scrape_direct(&settings).expect("followed");
        assert_eq!(scraped.batch.num_rows(), 1);
    }

    #[test]
    fn urls_are_redacted_for_logs() {
        assert_eq!(
            redact_url("https://user:pw@h:9100/metrics?token=x#f"),
            "https://h:9100/metrics"
        );
        assert_eq!(redact_url("http://h/metrics"), "http://h/metrics");
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
}
