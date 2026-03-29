use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Atomic stats structures (lock-free, hot-path friendly)
// ---------------------------------------------------------------------------

/// Stats for one component. Lock-free, readable from any thread.
pub struct ComponentStats {
    pub lines_total: AtomicU64,
    pub bytes_total: AtomicU64,
    pub errors_total: AtomicU64,
}

impl ComponentStats {
    pub fn new() -> Self {
        Self {
            lines_total: AtomicU64::new(0),
            bytes_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
        }
    }

    pub fn inc_lines(&self, n: u64) {
        self.lines_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_bytes(&self, n: u64) {
        self.bytes_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_errors(&self) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
    }

    fn lines(&self) -> u64 {
        self.lines_total.load(Ordering::Relaxed)
    }

    fn bytes(&self) -> u64 {
        self.bytes_total.load(Ordering::Relaxed)
    }

    fn errors(&self) -> u64 {
        self.errors_total.load(Ordering::Relaxed)
    }
}

impl Default for ComponentStats {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Pipeline-level metrics (shared between pipeline thread and diagnostics)
// ---------------------------------------------------------------------------

/// Stats for a full pipeline.
pub struct PipelineMetrics {
    pub name: String,
    /// (name, type, stats)
    pub inputs: Vec<(String, String, Arc<ComponentStats>)>,
    pub transform_sql: String,
    pub transform_in: Arc<ComponentStats>,
    pub transform_out: Arc<ComponentStats>,
    pub transform_errors: AtomicU64,
    /// (name, type, stats)
    pub outputs: Vec<(String, String, Arc<ComponentStats>)>,
    pub backpressure_stalls: AtomicU64,
    // Batch-level metrics
    pub batches_total: AtomicU64,
    pub batch_rows_total: AtomicU64,
    pub flush_by_size: AtomicU64,
    pub flush_by_timeout: AtomicU64,
    // Per-stage cumulative timing (nanoseconds)
    pub scan_nanos_total: AtomicU64,
    pub transform_nanos_total: AtomicU64,
    pub output_nanos_total: AtomicU64,
}

impl PipelineMetrics {
    pub fn new(name: impl Into<String>, transform_sql: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            inputs: Vec::new(),
            transform_sql: transform_sql.into(),
            transform_in: Arc::new(ComponentStats::new()),
            transform_out: Arc::new(ComponentStats::new()),
            transform_errors: AtomicU64::new(0),
            outputs: Vec::new(),
            backpressure_stalls: AtomicU64::new(0),
            batches_total: AtomicU64::new(0),
            batch_rows_total: AtomicU64::new(0),
            flush_by_size: AtomicU64::new(0),
            flush_by_timeout: AtomicU64::new(0),
            scan_nanos_total: AtomicU64::new(0),
            transform_nanos_total: AtomicU64::new(0),
            output_nanos_total: AtomicU64::new(0),
        }
    }

    pub fn add_input(
        &mut self,
        name: impl Into<String>,
        typ: impl Into<String>,
    ) -> Arc<ComponentStats> {
        let stats = Arc::new(ComponentStats::new());
        self.inputs
            .push((name.into(), typ.into(), Arc::clone(&stats)));
        stats
    }

    pub fn add_output(
        &mut self,
        name: impl Into<String>,
        typ: impl Into<String>,
    ) -> Arc<ComponentStats> {
        let stats = Arc::new(ComponentStats::new());
        self.outputs
            .push((name.into(), typ.into(), Arc::clone(&stats)));
        stats
    }

    /// Increment error counter on all outputs (used when a FanOut or single
    /// sink fails — we don't know which one, so count it on all).
    pub fn output_error(&self) {
        for (_, _, stats) in &self.outputs {
            stats.inc_errors();
        }
    }
}

// ---------------------------------------------------------------------------
// Diagnostics HTTP server
// ---------------------------------------------------------------------------

const VERSION: &str = "0.2.0";
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Lightweight diagnostics HTTP server. Runs on a dedicated thread, reads
/// atomic counters — no locking on the hot path.
pub struct DiagnosticsServer {
    pipelines: Vec<Arc<PipelineMetrics>>,
    start_time: Instant,
    bind_addr: String,
}

impl DiagnosticsServer {
    pub fn new(bind_addr: &str) -> Self {
        Self {
            pipelines: Vec::new(),
            start_time: Instant::now(),
            bind_addr: bind_addr.to_string(),
        }
    }

    pub fn add_pipeline(&mut self, metrics: Arc<PipelineMetrics>) {
        self.pipelines.push(metrics);
    }

    /// Spawn the server on a background thread. Returns the join handle.
    pub fn start(self) -> JoinHandle<()> {
        thread::spawn(move || self.run())
    }

    fn run(&self) {
        let server = tiny_http::Server::http(&self.bind_addr)
            .expect("diagnostics: failed to bind HTTP server");

        for request in server.incoming_requests() {
            let _ = self.handle_request(request);
        }
    }

    fn handle_request(
        &self,
        request: tiny_http::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let path = request.url().to_string();
        // Strip query string for routing.
        let route = path.split('?').next().unwrap_or(&path);

        match route {
            "/" => self.serve_dashboard(request),
            "/health" => self.serve_health(request),
            "/api/pipelines" => self.serve_pipelines(request),
            "/metrics" => self.serve_metrics(request),
            _ => {
                let resp = tiny_http::Response::from_string("not found")
                    .with_status_code(404)
                    .with_header(
                        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..])
                            .unwrap(),
                    );
                request.respond(resp)?;
                Ok(())
            }
        }
    }

    // -- endpoint handlers --------------------------------------------------

    fn serve_dashboard(
        &self,
        request: tiny_http::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let resp = tiny_http::Response::from_string(DASHBOARD_HTML).with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .unwrap(),
        );
        request.respond(resp)?;
        Ok(())
    }

    fn serve_health(&self, request: tiny_http::Request) -> Result<(), Box<dyn std::error::Error>> {
        let uptime = self.start_time.elapsed().as_secs();
        let body = format!(
            r#"{{"status":"ok","uptime_seconds":{},"version":"{}"}}"#,
            uptime, VERSION,
        );
        let resp = tiny_http::Response::from_string(body).with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
        );
        request.respond(resp)?;
        Ok(())
    }

    fn serve_pipelines(
        &self,
        request: tiny_http::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let uptime = self.start_time.elapsed().as_secs();
        let mut pipelines_json = Vec::new();

        for pm in &self.pipelines {
            let inputs_json: Vec<String> = pm
                .inputs
                .iter()
                .map(|(name, typ, stats)| {
                    format!(
                        r#"{{"name":"{}","type":"{}","lines_total":{},"bytes_total":{},"errors":{}}}"#,
                        esc(name),
                        esc(typ),
                        stats.lines(),
                        stats.bytes(),
                        stats.errors(),
                    )
                })
                .collect();

            let lines_in = pm.transform_in.lines();
            let lines_out = pm.transform_out.lines();
            let drop_rate = if lines_in > 0 {
                1.0 - (lines_out as f64 / lines_in as f64)
            } else {
                0.0
            };

            let outputs_json: Vec<String> = pm
                .outputs
                .iter()
                .map(|(name, typ, stats)| {
                    format!(
                        r#"{{"name":"{}","type":"{}","lines_total":{},"bytes_total":{},"errors":{}}}"#,
                        esc(name),
                        esc(typ),
                        stats.lines(),
                        stats.bytes(),
                        stats.errors(),
                    )
                })
                .collect();

            let batches = pm.batches_total.load(Ordering::Relaxed);
            let batch_rows = pm.batch_rows_total.load(Ordering::Relaxed);
            let avg_rows = if batches > 0 {
                batch_rows as f64 / batches as f64
            } else {
                0.0
            };
            let scan_s = pm.scan_nanos_total.load(Ordering::Relaxed) as f64 / 1e9;
            let transform_s = pm.transform_nanos_total.load(Ordering::Relaxed) as f64 / 1e9;
            let output_s = pm.output_nanos_total.load(Ordering::Relaxed) as f64 / 1e9;

            pipelines_json.push(format!(
                r#"{{"name":"{}","inputs":[{}],"transform":{{"sql":"{}","lines_in":{},"lines_out":{},"errors":{},"filter_drop_rate":{:.3}}},"outputs":[{}],"batches":{{"total":{},"avg_rows":{:.1},"flush_by_size":{},"flush_by_timeout":{}}},"stage_seconds":{{"scan":{:.6},"transform":{:.6},"output":{:.6}}}}}"#,
                esc(&pm.name),
                inputs_json.join(","),
                esc(&pm.transform_sql),
                lines_in,
                lines_out,
                pm.transform_errors.load(Ordering::Relaxed),
                drop_rate,
                outputs_json.join(","),
                batches,
                avg_rows,
                pm.flush_by_size.load(Ordering::Relaxed),
                pm.flush_by_timeout.load(Ordering::Relaxed),
                scan_s,
                transform_s,
                output_s,
            ));
        }

        let body = format!(
            r#"{{"pipelines":[{}],"system":{{"uptime_seconds":{},"version":"{}"}}}}"#,
            pipelines_json.join(","),
            uptime,
            VERSION,
        );

        let resp = tiny_http::Response::from_string(body).with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
        );
        request.respond(resp)?;
        Ok(())
    }

    fn serve_metrics(&self, request: tiny_http::Request) -> Result<(), Box<dyn std::error::Error>> {
        let mut out = String::with_capacity(2048);

        // Input lines
        out.push_str("# HELP logfwd_input_lines_total Total lines read per input\n");
        out.push_str("# TYPE logfwd_input_lines_total counter\n");
        for pm in &self.pipelines {
            for (name, _typ, stats) in &pm.inputs {
                out.push_str(&format!(
                    "logfwd_input_lines_total{{pipeline=\"{}\",input=\"{}\"}} {}\n",
                    esc(&pm.name),
                    esc(name),
                    stats.lines(),
                ));
            }
        }

        // Input bytes
        out.push_str("\n# HELP logfwd_input_bytes_total Total bytes read per input\n");
        out.push_str("# TYPE logfwd_input_bytes_total counter\n");
        for pm in &self.pipelines {
            for (name, _typ, stats) in &pm.inputs {
                out.push_str(&format!(
                    "logfwd_input_bytes_total{{pipeline=\"{}\",input=\"{}\"}} {}\n",
                    esc(&pm.name),
                    esc(name),
                    stats.bytes(),
                ));
            }
        }

        // Output lines
        out.push_str("\n# HELP logfwd_output_lines_total Total lines sent per output\n");
        out.push_str("# TYPE logfwd_output_lines_total counter\n");
        for pm in &self.pipelines {
            for (name, _typ, stats) in &pm.outputs {
                out.push_str(&format!(
                    "logfwd_output_lines_total{{pipeline=\"{}\",output=\"{}\"}} {}\n",
                    esc(&pm.name),
                    esc(name),
                    stats.lines(),
                ));
            }
        }

        // Output bytes
        out.push_str("\n# HELP logfwd_output_bytes_total Total bytes sent per output\n");
        out.push_str("# TYPE logfwd_output_bytes_total counter\n");
        for pm in &self.pipelines {
            for (name, _typ, stats) in &pm.outputs {
                out.push_str(&format!(
                    "logfwd_output_bytes_total{{pipeline=\"{}\",output=\"{}\"}} {}\n",
                    esc(&pm.name),
                    esc(name),
                    stats.bytes(),
                ));
            }
        }

        // Transform lines in/out
        out.push_str("\n# HELP logfwd_transform_lines_in Lines entering transform\n");
        out.push_str("# TYPE logfwd_transform_lines_in counter\n");
        for pm in &self.pipelines {
            out.push_str(&format!(
                "logfwd_transform_lines_in{{pipeline=\"{}\"}} {}\n",
                esc(&pm.name),
                pm.transform_in.lines(),
            ));
        }

        out.push_str("\n# HELP logfwd_transform_lines_out Lines exiting transform\n");
        out.push_str("# TYPE logfwd_transform_lines_out counter\n");
        for pm in &self.pipelines {
            out.push_str(&format!(
                "logfwd_transform_lines_out{{pipeline=\"{}\"}} {}\n",
                esc(&pm.name),
                pm.transform_out.lines(),
            ));
        }

        // Output errors
        out.push_str("\n# HELP logfwd_output_errors_total Send errors per output\n");
        out.push_str("# TYPE logfwd_output_errors_total counter\n");
        for pm in &self.pipelines {
            for (name, _typ, stats) in &pm.outputs {
                out.push_str(&format!(
                    "logfwd_output_errors_total{{pipeline=\"{}\",output=\"{}\"}} {}\n",
                    esc(&pm.name),
                    esc(name),
                    stats.errors(),
                ));
            }
        }

        // Transform errors
        out.push_str("\n# HELP logfwd_transform_errors_total SQL transform execution errors\n");
        out.push_str("# TYPE logfwd_transform_errors_total counter\n");
        for pm in &self.pipelines {
            out.push_str(&format!(
                "logfwd_transform_errors_total{{pipeline=\"{}\"}} {}\n",
                esc(&pm.name),
                pm.transform_errors.load(Ordering::Relaxed),
            ));
        }

        // Batch metrics
        out.push_str("\n# HELP logfwd_batches_total Total batches processed\n");
        out.push_str("# TYPE logfwd_batches_total counter\n");
        for pm in &self.pipelines {
            out.push_str(&format!(
                "logfwd_batches_total{{pipeline=\"{}\"}} {}\n",
                esc(&pm.name),
                pm.batches_total.load(Ordering::Relaxed),
            ));
        }

        out.push_str("\n# HELP logfwd_batch_rows_total Total rows across all batches\n");
        out.push_str("# TYPE logfwd_batch_rows_total counter\n");
        for pm in &self.pipelines {
            out.push_str(&format!(
                "logfwd_batch_rows_total{{pipeline=\"{}\"}} {}\n",
                esc(&pm.name),
                pm.batch_rows_total.load(Ordering::Relaxed),
            ));
        }

        // Flush reason
        out.push_str("\n# HELP logfwd_flush_reason_total Batch flushes by trigger reason\n");
        out.push_str("# TYPE logfwd_flush_reason_total counter\n");
        for pm in &self.pipelines {
            out.push_str(&format!(
                "logfwd_flush_reason_total{{pipeline=\"{}\",reason=\"size\"}} {}\n",
                esc(&pm.name),
                pm.flush_by_size.load(Ordering::Relaxed),
            ));
            out.push_str(&format!(
                "logfwd_flush_reason_total{{pipeline=\"{}\",reason=\"timeout\"}} {}\n",
                esc(&pm.name),
                pm.flush_by_timeout.load(Ordering::Relaxed),
            ));
        }

        // Stage latency (cumulative seconds — divide by batches_total for avg)
        out.push_str(
            "\n# HELP logfwd_stage_seconds_total Cumulative time spent in each pipeline stage\n",
        );
        out.push_str("# TYPE logfwd_stage_seconds_total counter\n");
        for pm in &self.pipelines {
            let scan_s = pm.scan_nanos_total.load(Ordering::Relaxed) as f64 / 1e9;
            let transform_s = pm.transform_nanos_total.load(Ordering::Relaxed) as f64 / 1e9;
            let output_s = pm.output_nanos_total.load(Ordering::Relaxed) as f64 / 1e9;
            out.push_str(&format!(
                "logfwd_stage_seconds_total{{pipeline=\"{}\",stage=\"scan\"}} {:.6}\n",
                esc(&pm.name),
                scan_s,
            ));
            out.push_str(&format!(
                "logfwd_stage_seconds_total{{pipeline=\"{}\",stage=\"transform\"}} {:.6}\n",
                esc(&pm.name),
                transform_s,
            ));
            out.push_str(&format!(
                "logfwd_stage_seconds_total{{pipeline=\"{}\",stage=\"output\"}} {:.6}\n",
                esc(&pm.name),
                output_s,
            ));
        }

        // Backpressure stalls
        out.push_str(
            "\n# HELP logfwd_backpressure_stalls_total Times reader blocked on full channel\n",
        );
        out.push_str("# TYPE logfwd_backpressure_stalls_total counter\n");
        for pm in &self.pipelines {
            out.push_str(&format!(
                "logfwd_backpressure_stalls_total{{pipeline=\"{}\"}} {}\n",
                esc(&pm.name),
                pm.backpressure_stalls.load(Ordering::Relaxed),
            ));
        }

        let resp = tiny_http::Response::from_string(out).with_header(
            tiny_http::Header::from_bytes(
                &b"Content-Type"[..],
                &b"text/plain; version=0.0.4; charset=utf-8"[..],
            )
            .unwrap(),
        );
        request.respond(resp)?;
        Ok(())
    }
}

/// Minimal JSON-string escaping (backslash, double-quote, control chars).
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    /// Pick an available port by binding to :0.
    fn free_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    /// Build a server with one pipeline pre-populated with known counter values.
    fn server_with_test_pipeline(port: u16) -> DiagnosticsServer {
        let mut pm = PipelineMetrics::new("default", "SELECT * FROM logs WHERE level != 'DEBUG'");

        let inp = pm.add_input("pod_logs", "file");
        inp.inc_lines(1000);
        inp.inc_bytes(50000);

        pm.transform_in.inc_lines(1000);
        pm.transform_out.inc_lines(900);

        let out = pm.add_output("collector", "otlp");
        out.inc_lines(900);
        out.inc_bytes(30000);
        out.inc_errors();
        out.inc_errors();

        // Batch-level metrics.
        pm.batches_total.store(50, Ordering::Relaxed);
        pm.batch_rows_total.store(4500, Ordering::Relaxed);
        pm.flush_by_size.store(30, Ordering::Relaxed);
        pm.flush_by_timeout.store(20, Ordering::Relaxed);
        pm.scan_nanos_total.store(100_000_000, Ordering::Relaxed); // 0.1s
        pm.transform_nanos_total
            .store(500_000_000, Ordering::Relaxed); // 0.5s
        pm.output_nanos_total.store(200_000_000, Ordering::Relaxed); // 0.2s
        pm.transform_errors.store(3, Ordering::Relaxed);

        let mut server = DiagnosticsServer::new(&format!("127.0.0.1:{}", port));
        server.add_pipeline(Arc::new(pm));
        server
    }

    /// Simple HTTP GET helper using raw TCP.
    fn http_get(port: u16, path: &str) -> (u16, String) {
        use std::io::Write;
        use std::net::TcpStream;

        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).expect("connect failed");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .ok();
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            path
        );
        stream.write_all(req.as_bytes()).unwrap();

        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf).to_string();

        // Parse status code from first line.
        let status = text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);

        // Split headers from body.
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();

        (status, body)
    }

    #[test]
    fn test_component_stats() {
        let stats = ComponentStats::new();
        assert_eq!(stats.lines(), 0);
        assert_eq!(stats.bytes(), 0);
        assert_eq!(stats.errors(), 0);

        stats.inc_lines(10);
        stats.inc_lines(5);
        assert_eq!(stats.lines(), 15);

        stats.inc_bytes(1024);
        stats.inc_bytes(2048);
        assert_eq!(stats.bytes(), 3072);

        stats.inc_errors();
        stats.inc_errors();
        stats.inc_errors();
        assert_eq!(stats.errors(), 3);
    }

    #[test]
    fn test_health_endpoint() {
        let port = free_port();
        let server = server_with_test_pipeline(port);
        let _handle = server.start();

        // Give the server a moment to bind.
        std::thread::sleep(std::time::Duration::from_millis(100));

        let (status, body) = http_get(port, "/health");
        assert_eq!(status, 200);
        assert!(body.contains(r#""status":"ok""#), "body: {}", body);
        assert!(body.contains(r#""version":"0.2.0""#), "body: {}", body);
        assert!(body.contains(r#""uptime_seconds":"#), "body: {}", body);
    }

    #[test]
    fn test_metrics_endpoint() {
        let port = free_port();
        let server = server_with_test_pipeline(port);
        let _handle = server.start();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let (status, body) = http_get(port, "/metrics");
        assert_eq!(status, 200);

        // Verify Prometheus exposition format.
        assert!(
            body.contains(r#"logfwd_input_lines_total{pipeline="default",input="pod_logs"} 1000"#),
            "body: {}",
            body,
        );
        assert!(
            body.contains(
                r#"logfwd_output_lines_total{pipeline="default",output="collector"} 900"#
            ),
            "body: {}",
            body,
        );
        assert!(
            body.contains(r#"logfwd_transform_lines_in{pipeline="default"} 1000"#),
            "body: {}",
            body,
        );
        assert!(
            body.contains(r#"logfwd_transform_lines_out{pipeline="default"} 900"#),
            "body: {}",
            body,
        );
        assert!(
            body.contains(r#"logfwd_output_errors_total{pipeline="default",output="collector"} 2"#),
            "body: {}",
            body,
        );
        assert!(
            body.contains(r#"logfwd_backpressure_stalls_total{pipeline="default"} 0"#),
            "body: {}",
            body,
        );

        // New batch/stage metrics.
        assert!(
            body.contains(r#"logfwd_batches_total{pipeline="default"} 50"#),
            "body: {}",
            body,
        );
        assert!(
            body.contains(r#"logfwd_batch_rows_total{pipeline="default"} 4500"#),
            "body: {}",
            body,
        );
        assert!(
            body.contains(r#"logfwd_flush_reason_total{pipeline="default",reason="size"} 30"#),
            "body: {}",
            body,
        );
        assert!(
            body.contains(r#"logfwd_flush_reason_total{pipeline="default",reason="timeout"} 20"#),
            "body: {}",
            body,
        );
        assert!(
            body.contains(r#"logfwd_transform_errors_total{pipeline="default"} 3"#),
            "body: {}",
            body,
        );
        assert!(
            body.contains(r#"logfwd_stage_seconds_total{pipeline="default",stage="scan"} 0.1"#),
            "body: {}",
            body,
        );
        assert!(
            body.contains(
                r#"logfwd_stage_seconds_total{pipeline="default",stage="transform"} 0.5"#
            ),
            "body: {}",
            body,
        );
        assert!(
            body.contains(r#"logfwd_stage_seconds_total{pipeline="default",stage="output"} 0.2"#),
            "body: {}",
            body,
        );

        // Check HELP/TYPE metadata present.
        assert!(body.contains("# HELP logfwd_input_lines_total"));
        assert!(body.contains("# TYPE logfwd_input_lines_total counter"));
        assert!(body.contains("# HELP logfwd_stage_seconds_total"));
        assert!(body.contains("# HELP logfwd_batches_total"));
        assert!(body.contains("# HELP logfwd_flush_reason_total"));
    }

    #[test]
    fn test_pipelines_endpoint() {
        let port = free_port();
        let server = server_with_test_pipeline(port);
        let _handle = server.start();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let (status, body) = http_get(port, "/api/pipelines");
        assert_eq!(status, 200);
        assert!(body.contains(r#""name":"default""#), "body: {}", body);
        assert!(body.contains(r#""lines_total":1000"#), "body: {}", body);
        assert!(body.contains(r#""lines_in":1000"#), "body: {}", body);
        assert!(body.contains(r#""lines_out":900"#), "body: {}", body);
        assert!(body.contains(r#""errors":3"#), "body: {}", body);
        assert!(body.contains(r#""total":50"#), "body: {}", body);
        assert!(body.contains(r#""avg_rows":90.0"#), "body: {}", body);
        assert!(body.contains(r#""flush_by_size":30"#), "body: {}", body);
        assert!(body.contains(r#""flush_by_timeout":20"#), "body: {}", body);
        assert!(body.contains(r#""version":"0.2.0""#), "body: {}", body);
    }

    #[test]
    fn test_not_found() {
        let port = free_port();
        let server = server_with_test_pipeline(port);
        let _handle = server.start();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let (status, _body) = http_get(port, "/nonexistent");
        assert_eq!(status, 404);
    }
}
