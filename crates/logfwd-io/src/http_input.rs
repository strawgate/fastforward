//! HTTP input source for newline-delimited payload ingestion.
//!
//! Listens for requests on a configurable route (default `/`), accepts
//! optionally compressed request bodies, and forwards newline-delimited bytes
//! to the pipeline scanner path as [`InputEvent::Data`].

use std::io;
use std::io::Read as _;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use flate2::read::GzDecoder;
use logfwd_types::diagnostics::ComponentHealth;

use crate::InputError;
use crate::input::{InputEvent, InputSource};

/// Default max request body size (20 MiB).
const DEFAULT_MAX_REQUEST_BODY_SIZE: usize = 20 * 1024 * 1024;

/// Bounded channel capacity — limits memory when the pipeline falls behind.
const CHANNEL_BOUND: usize = 4096;

/// Accepted HTTP method for the input endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpInputMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Head,
    Options,
}

impl HttpInputMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }

    #[must_use]
    pub fn matches(self, method: &tiny_http::Method) -> bool {
        matches!(
            (self, method),
            (Self::Get, tiny_http::Method::Get)
                | (Self::Post, tiny_http::Method::Post)
                | (Self::Put, tiny_http::Method::Put)
                | (Self::Delete, tiny_http::Method::Delete)
                | (Self::Patch, tiny_http::Method::Patch)
                | (Self::Head, tiny_http::Method::Head)
                | (Self::Options, tiny_http::Method::Options)
        )
    }
}

/// HTTP input behavior and limits.
#[derive(Debug, Clone)]
pub struct HttpInputOptions {
    /// Route path to match, e.g. `/ingest`.
    pub path: String,
    /// When true, only exact path matches are accepted.
    pub strict_path: bool,
    /// Accepted HTTP method for ingest requests.
    pub method: HttpInputMethod,
    /// Max request body size in bytes.
    pub max_request_body_size: usize,
    /// HTTP response code for accepted requests.
    pub response_code: u16,
}

impl Default for HttpInputOptions {
    fn default() -> Self {
        Self {
            path: "/".to_string(),
            strict_path: true,
            method: HttpInputMethod::Post,
            max_request_body_size: DEFAULT_MAX_REQUEST_BODY_SIZE,
            response_code: 200,
        }
    }
}

/// HTTP NDJSON receiver that forwards bytes to the scanner pipeline.
pub struct HttpInput {
    name: String,
    rx: Option<mpsc::Receiver<Vec<u8>>>,
    /// The address the HTTP server is bound to.
    addr: std::net::SocketAddr,
    server: Arc<tiny_http::Server>,
    /// Keep the server thread handle alive.
    handle: Option<std::thread::JoinHandle<()>>,
    /// Shutdown mechanism for the background thread.
    is_running: Arc<AtomicBool>,
    /// Source-owned health snapshot for readiness and diagnostics.
    health: Arc<AtomicU8>,
}

impl HttpInput {
    /// Bind an HTTP server on `addr` (e.g. "0.0.0.0:8081").
    /// Uses default options with `path` override when supplied.
    pub fn new(name: impl Into<String>, addr: &str, path: Option<&str>) -> io::Result<Self> {
        let mut options = HttpInputOptions::default();
        if let Some(path) = path {
            options.path = path.to_string();
        }
        Self::new_with_options(name, addr, options)
    }

    /// Bind an HTTP server with explicit options.
    pub fn new_with_options(
        name: impl Into<String>,
        addr: &str,
        options: HttpInputOptions,
    ) -> io::Result<Self> {
        Self::new_with_capacity_inner(name, addr, CHANNEL_BOUND, options)
    }

    #[cfg(test)]
    fn new_with_capacity(
        name: impl Into<String>,
        addr: &str,
        capacity: usize,
        options: HttpInputOptions,
    ) -> io::Result<Self> {
        Self::new_with_capacity_inner(name, addr, capacity, options)
    }

    fn new_with_capacity_inner(
        name: impl Into<String>,
        addr: &str,
        capacity: usize,
        options: HttpInputOptions,
    ) -> io::Result<Self> {
        let options = normalize_options(options)?;
        let route_path = options.path.clone();
        let strict_path = options.strict_path;
        let accepted_method = options.method;
        let max_request_body_size = options.max_request_body_size;
        let success_response_code = options.response_code;

        let server = Arc::new(
            tiny_http::Server::http(addr)
                .map_err(|e| io::Error::other(format!("HTTP input bind {addr}: {e}")))?,
        );

        let bound_addr = match server.server_addr() {
            tiny_http::ListenAddr::IP(a) => a,
            tiny_http::ListenAddr::Unix(_) => {
                return Err(io::Error::other("HTTP input: unexpected listen addr"));
            }
        };

        let (tx, rx) = mpsc::sync_channel(capacity);

        let server_clone = Arc::clone(&server);
        let is_running = Arc::new(AtomicBool::new(true));
        let is_running_clone = Arc::clone(&is_running);
        let health = Arc::new(AtomicU8::new(ComponentHealth::Healthy.as_repr()));
        let health_clone = Arc::clone(&health);

        let handle = std::thread::Builder::new()
            .name("http-input".into())
            .spawn(move || {
                while is_running_clone.load(Ordering::Relaxed) {
                    let mut request = match server_clone.recv_timeout(Duration::from_millis(100)) {
                        Ok(Some(req)) => req,
                        Ok(None) => continue,
                        Err(_) => {
                            health_clone
                                .store(ComponentHealth::Failed.as_repr(), Ordering::Relaxed);
                            break;
                        }
                    };

                    let url = request.url().to_string();
                    let path_only = url.split('?').next().unwrap_or(&url);
                    if !path_matches(path_only, &route_path, strict_path) {
                        let _ = request.respond(
                            tiny_http::Response::from_string("not found").with_status_code(404),
                        );
                        continue;
                    }

                    if !accepted_method.matches(request.method()) {
                        let allow_header = format!("Allow: {}", accepted_method.as_str())
                            .parse::<tiny_http::Header>()
                            .expect("static method header is valid");
                        let _ = request.respond(
                            tiny_http::Response::from_string("method not allowed")
                                .with_status_code(405)
                                .with_header(allow_header),
                        );
                        continue;
                    }

                    if request.body_length().unwrap_or(0) > max_request_body_size {
                        let _ = request.respond(
                            tiny_http::Response::from_string("payload too large")
                                .with_status_code(413),
                        );
                        continue;
                    }

                    let mut body = Vec::with_capacity(
                        request.body_length().unwrap_or(0).min(max_request_body_size),
                    );
                    match request
                        .as_reader()
                        .take(max_request_body_size as u64 + 1)
                        .read_to_end(&mut body)
                    {
                        Ok(n) if n > max_request_body_size => {
                            let _ = request.respond(
                                tiny_http::Response::from_string("payload too large")
                                    .with_status_code(413),
                            );
                            continue;
                        }
                        Err(_) => {
                            let _ = request.respond(
                                tiny_http::Response::from_string("read error")
                                    .with_status_code(400),
                            );
                            continue;
                        }
                        Ok(_) => {}
                    }

                    let content_encoding = request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("Content-Encoding"))
                        .map(|h| h.value.as_str().to_lowercase());

                    let mut body =
                        match decode_content(body, content_encoding.as_deref(), max_request_body_size)
                        {
                            Ok(decoded) => decoded,
                            Err(InputError::Receiver(msg)) => {
                                let _ = request.respond(
                                    tiny_http::Response::from_string(msg).with_status_code(400),
                                );
                                continue;
                            }
                            Err(InputError::Io(e)) if e.kind() == io::ErrorKind::InvalidData => {
                                let _ = request.respond(
                                    tiny_http::Response::from_string(e.to_string())
                                        .with_status_code(413),
                                );
                                continue;
                            }
                            Err(_) => {
                                let _ = request.respond(
                                    tiny_http::Response::from_string("decode failed")
                                        .with_status_code(400),
                                );
                                continue;
                            }
                        };

                    if body.is_empty() {
                        health_clone.store(ComponentHealth::Healthy.as_repr(), Ordering::Relaxed);
                        let _ = request.respond(tiny_http::Response::empty(success_response_code));
                        continue;
                    }

                    if !body.ends_with(b"\n") {
                        body.push(b'\n');
                    }

                    match tx.try_send(body) {
                        Ok(()) => {
                            health_clone
                                .store(ComponentHealth::Healthy.as_repr(), Ordering::Relaxed);
                            let _ = request.respond(tiny_http::Response::empty(success_response_code));
                        }
                        Err(mpsc::TrySendError::Full(_)) => {
                            health_clone
                                .store(ComponentHealth::Degraded.as_repr(), Ordering::Relaxed);
                            let _ = request.respond(
                                tiny_http::Response::from_string(
                                    "too many requests: pipeline backpressure",
                                )
                                .with_status_code(429),
                            );
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => {
                            if is_running_clone.load(Ordering::Relaxed) {
                                health_clone
                                    .store(ComponentHealth::Failed.as_repr(), Ordering::Relaxed);
                            }
                            let _ = request.respond(
                                tiny_http::Response::from_string(
                                    "service unavailable: pipeline disconnected",
                                )
                                .with_status_code(503),
                            );
                        }
                    }
                }
            })
            .map_err(io::Error::other)?;

        Ok(Self {
            name: name.into(),
            rx: Some(rx),
            addr: bound_addr,
            server,
            handle: Some(handle),
            is_running,
            health,
        })
    }

    /// Returns the local address the HTTP server is bound to.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.addr
    }
}

impl Drop for HttpInput {
    fn drop(&mut self) {
        self.health
            .store(ComponentHealth::Stopping.as_repr(), Ordering::Relaxed);
        self.is_running.store(false, Ordering::Relaxed);
        self.rx.take();
        self.server.unblock();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.health
            .store(ComponentHealth::Stopped.as_repr(), Ordering::Relaxed);
    }
}

impl InputSource for HttpInput {
    fn poll(&mut self) -> io::Result<Vec<InputEvent>> {
        let Some(rx) = self.rx.as_ref() else {
            return Ok(vec![]);
        };

        let mut all = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            all.extend_from_slice(&bytes);
        }

        if all.is_empty() {
            return Ok(vec![]);
        }
        Ok(vec![InputEvent::Data {
            bytes: all,
            source_id: None,
        }])
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn health(&self) -> ComponentHealth {
        let stored = ComponentHealth::from_repr(self.health.load(Ordering::Relaxed));
        if self
            .handle
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
            && self.is_running.load(Ordering::Relaxed)
        {
            ComponentHealth::Failed
        } else {
            stored
        }
    }
}

fn normalize_options(mut options: HttpInputOptions) -> io::Result<HttpInputOptions> {
    options.path = normalize_route(&options.path)?;
    if options.max_request_body_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "http input max_request_body_size must be >= 1",
        ));
    }
    if !is_valid_success_response_code(options.response_code) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "http input response_code must be one of: 200, 201, 202, 204",
        ));
    }
    Ok(options)
}

fn is_valid_success_response_code(code: u16) -> bool {
    matches!(code, 200 | 201 | 202 | 204)
}

fn normalize_route(path: &str) -> io::Result<String> {
    if path.trim().is_empty() || !path.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "http input path must start with '/'",
        ));
    }
    Ok(path.to_string())
}

fn path_matches(path: &str, route: &str, strict_path: bool) -> bool {
    if strict_path {
        return path == route;
    }
    if route == "/" {
        return true;
    }
    path == route || path.strip_prefix(route).is_some_and(|suffix| suffix.starts_with('/'))
}

fn decode_content(
    body: Vec<u8>,
    content_encoding: Option<&str>,
    max_request_body_size: usize,
) -> Result<Vec<u8>, InputError> {
    match content_encoding {
        Some("zstd") => decompress_zstd(&body, max_request_body_size),
        Some("gzip") => decompress_gzip(&body, max_request_body_size),
        None | Some("identity") => Ok(body),
        Some(other) => Err(InputError::Receiver(format!(
            "unsupported content-encoding: {other}"
        ))),
    }
}

fn decompress_zstd(body: &[u8], max_request_body_size: usize) -> Result<Vec<u8>, InputError> {
    let decoder = zstd::Decoder::new(body)
        .map_err(|_| InputError::Receiver("zstd decompression failed".to_string()))?;
    read_decompressed_body(
        decoder,
        body.len(),
        max_request_body_size,
        "zstd decompression failed",
    )
}

fn decompress_gzip(body: &[u8], max_request_body_size: usize) -> Result<Vec<u8>, InputError> {
    let decoder = GzDecoder::new(body);
    read_decompressed_body(
        decoder,
        body.len(),
        max_request_body_size,
        "gzip decompression failed",
    )
}

fn read_decompressed_body(
    reader: impl io::Read,
    compressed_len: usize,
    max_request_body_size: usize,
    error_label: &str,
) -> Result<Vec<u8>, InputError> {
    let mut decompressed = Vec::with_capacity(compressed_len.min(max_request_body_size));
    match reader
        .take(max_request_body_size as u64 + 1)
        .read_to_end(&mut decompressed)
    {
        Ok(n) if n > max_request_body_size => Err(InputError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload too large",
        ))),
        Ok(_) => Ok(decompressed),
        Err(_) => Err(InputError::Receiver(error_label.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn poll_until_data(input: &mut dyn InputSource, timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let mut out = Vec::new();
            for event in input.poll().expect("poll should succeed") {
                if let InputEvent::Data { bytes, .. } = event {
                    out.extend_from_slice(&bytes);
                }
            }
            if !out.is_empty() {
                return out;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        vec![]
    }

    #[test]
    fn http_ndjson_roundtrip() {
        let options = HttpInputOptions {
            path: "/ingest".to_string(),
            ..HttpInputOptions::default()
        };
        let mut input =
            HttpInput::new_with_options("test", "127.0.0.1:0", options).expect("http input binds");
        let url = format!("http://{}/ingest", input.local_addr());

        let body = b"{\"msg\":\"hello\"}\n{\"msg\":\"world\"}\n";
        let resp = ureq::post(&url)
            .header("Content-Type", "application/x-ndjson")
            .send(body)
            .expect("POST should succeed");
        assert_eq!(resp.status(), 200);

        let data = poll_until_data(&mut input, Duration::from_secs(2));
        let text = String::from_utf8_lossy(&data);
        assert!(text.contains("\"msg\":\"hello\""), "expected first row: {text}");
        assert!(text.contains("\"msg\":\"world\""), "expected second row: {text}");
    }

    #[test]
    fn http_default_route_accepts_root() {
        let mut input =
            HttpInput::new_with_options("test", "127.0.0.1:0", HttpInputOptions::default())
                .expect("http input binds");
        let url = format!("http://{}/", input.local_addr());

        let resp = ureq::post(&url).send(b"{\"msg\":\"root\"}").expect("POST should succeed");
        assert_eq!(resp.status(), 200);

        let data = poll_until_data(&mut input, Duration::from_secs(2));
        let text = String::from_utf8_lossy(&data);
        assert!(text.contains("\"msg\":\"root\""), "expected root row: {text}");
    }

    #[test]
    fn http_appends_newline_when_missing() {
        let options = HttpInputOptions {
            path: "/ingest".to_string(),
            ..HttpInputOptions::default()
        };
        let mut input =
            HttpInput::new_with_options("test", "127.0.0.1:0", options).expect("http input binds");
        let url = format!("http://{}/ingest", input.local_addr());

        let body = b"{\"msg\":\"no-newline\"}";
        let resp = ureq::post(&url).send(body).expect("POST should succeed");
        assert_eq!(resp.status(), 200);

        let data = poll_until_data(&mut input, Duration::from_secs(2));
        assert!(
            data.ends_with(b"\n"),
            "receiver must append trailing newline"
        );
    }

    #[test]
    fn http_rejects_wrong_path() {
        let options = HttpInputOptions {
            path: "/ingest".to_string(),
            ..HttpInputOptions::default()
        };
        let input =
            HttpInput::new_with_options("test", "127.0.0.1:0", options).expect("http input binds");
        let url = format!("http://{}/not-ingest", input.local_addr());

        let status = match ureq::post(&url).send(b"{\"x\":1}\n") {
            Ok(resp) => resp.status().as_u16(),
            Err(ureq::Error::StatusCode(code)) => code,
            Err(err) => panic!("unexpected request failure: {err}"),
        };
        assert_eq!(status, 404, "wrong path should return 404");
    }

    #[test]
    fn http_prefix_path_when_not_strict() {
        let options = HttpInputOptions {
            path: "/ingest".to_string(),
            strict_path: false,
            ..HttpInputOptions::default()
        };
        let mut input =
            HttpInput::new_with_options("test", "127.0.0.1:0", options).expect("http input binds");
        let url = format!("http://{}/ingest/team-a", input.local_addr());

        let resp = ureq::post(&url)
            .send(b"{\"msg\":\"prefix\"}\n")
            .expect("POST should succeed");
        assert_eq!(resp.status(), 200);

        let data = poll_until_data(&mut input, Duration::from_secs(2));
        let text = String::from_utf8_lossy(&data);
        assert!(text.contains("\"msg\":\"prefix\""), "expected prefix row: {text}");
    }

    #[test]
    fn http_rejects_method_mismatch() {
        let options = HttpInputOptions {
            path: "/ingest".to_string(),
            method: HttpInputMethod::Put,
            ..HttpInputOptions::default()
        };
        let input =
            HttpInput::new_with_options("test", "127.0.0.1:0", options).expect("http input binds");
        let url = format!("http://{}/ingest", input.local_addr());

        let status = match ureq::post(&url).send(b"{\"x\":1}\n") {
            Ok(resp) => resp.status().as_u16(),
            Err(ureq::Error::StatusCode(code)) => code,
            Err(err) => panic!("unexpected request failure: {err}"),
        };
        assert_eq!(status, 405, "POST should be rejected for PUT-only endpoint");
    }

    #[test]
    fn http_returns_429_when_channel_full() {
        let options = HttpInputOptions {
            path: "/ingest".to_string(),
            ..HttpInputOptions::default()
        };
        let mut input = HttpInput::new_with_capacity("test", "127.0.0.1:0", 1, options)
            .expect("http input binds");
        let url = format!("http://{}/ingest", input.local_addr());

        let first = ureq::post(&url).send(b"{\"seq\":1}\n").expect("first POST");
        assert_eq!(first.status(), 200);

        let status = match ureq::post(&url).send(b"{\"seq\":2}\n") {
            Ok(resp) => resp.status().as_u16(),
            Err(ureq::Error::StatusCode(code)) => code,
            Err(err) => panic!("unexpected request failure: {err}"),
        };
        assert!(
            status == 429 || status == 503,
            "expected backpressure response (429/503), got {status}"
        );

        let _ = poll_until_data(&mut input, Duration::from_secs(2));

        let third = ureq::post(&url)
            .send(b"{\"seq\":3}\n")
            .expect("third POST should succeed after drain");
        assert_eq!(third.status(), 200);
    }
}
