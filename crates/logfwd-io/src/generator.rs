//! Synthetic data generator input source.
//!
//! Produces JSON log lines at a configurable rate. Used for benchmarking
//! and testing pipelines without external data sources.

use std::io;
use std::io::Write;

use crate::input::{InputEvent, InputSource};

/// Controls the complexity/size of generated lines.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorComplexity {
    /// Flat JSON object, ~200 bytes per line.
    #[default]
    Simple,
    /// Includes occasional nested objects and arrays, ~400-800 bytes.
    Complex,
}

/// Named generator output profiles.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorProfile {
    /// Synthetic request-like JSON logs.
    #[default]
    Logs,
    /// Deterministic benchmark envelope with stable stream/event identity.
    Benchmark,
}

/// Configuration for the generator input.
pub struct GeneratorConfig {
    /// Target events per second. 0 = unlimited (as fast as possible).
    pub events_per_sec: u64,
    /// Number of events per batch (per poll() call).
    pub batch_size: usize,
    /// Total events to generate. 0 = infinite.
    pub total_events: u64,
    /// Controls the size and shape of generated JSON lines.
    pub complexity: GeneratorComplexity,
    /// Which event shape to emit.
    pub profile: GeneratorProfile,
    /// Benchmark run identifier included on every `benchmark` profile row.
    pub benchmark_id: Option<String>,
    /// Source identity included on `benchmark` rows.
    pub pod_name: Option<String>,
    /// Stable stream identity used for benchmark `event_id` generation.
    pub stream_id: Option<String>,
    /// Service name for `benchmark` rows. Defaults to `bench-emitter`.
    pub service: Option<String>,
}

impl Default for GeneratorConfig {
    fn default() -> Self {
        Self {
            events_per_sec: 0,
            batch_size: 1000,
            total_events: 0,
            complexity: GeneratorComplexity::default(),
            profile: GeneratorProfile::default(),
            benchmark_id: None,
            pod_name: None,
            stream_id: None,
            service: None,
        }
    }
}

/// Input source that generates synthetic JSON log lines.
pub struct GeneratorInput {
    name: String,
    config: GeneratorConfig,
    counter: u64,
    buf: Vec<u8>,
    done: bool,
    last_batch: std::time::Instant,
    benchmark_fields: BenchmarkFields,
}

const LEVELS: [&str; 4] = ["INFO", "DEBUG", "WARN", "ERROR"];
const PATHS: [&str; 5] = [
    "/api/v1/users",
    "/api/v1/orders",
    "/api/v2/products",
    "/health",
    "/api/v1/auth",
];
const METHODS: [&str; 4] = ["GET", "POST", "PUT", "DELETE"];
const SERVICES: [&str; 3] = ["myapp", "gateway", "auth-svc"];

#[derive(Debug, Clone)]
struct BenchmarkFields {
    benchmark_id: Option<String>,
    pod_name: String,
    stream_id: String,
    service: String,
}

impl GeneratorInput {
    pub fn new(name: impl Into<String>, config: GeneratorConfig) -> Self {
        let name = name.into();
        let stream_id = config
            .stream_id
            .clone()
            .or_else(|| config.pod_name.clone())
            .unwrap_or_else(|| name.clone());
        let pod_name = config.pod_name.clone().unwrap_or_else(|| stream_id.clone());
        let benchmark_fields = BenchmarkFields {
            benchmark_id: config.benchmark_id.clone(),
            pod_name,
            stream_id,
            service: config
                .service
                .clone()
                .unwrap_or_else(|| "bench-emitter".to_string()),
        };
        Self {
            name,
            buf: Vec::with_capacity(config.batch_size * 512),
            config,
            counter: 0,
            done: false,
            benchmark_fields,
            // Use a time far in the past so the first poll() always succeeds.
            last_batch: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(3600))
                .unwrap_or_else(std::time::Instant::now),
        }
    }

    /// Return the total number of events generated so far.
    pub fn events_generated(&self) -> u64 {
        self.counter
    }

    fn generate_batch(&mut self) {
        self.buf.clear();
        let n = self.config.batch_size;
        for _ in 0..n {
            if self.config.total_events > 0 && self.counter >= self.config.total_events {
                self.done = true;
                break;
            }
            self.write_event();
            self.buf.push(b'\n');
            self.counter += 1;
        }
    }

    fn write_event(&mut self) {
        match self.config.profile {
            GeneratorProfile::Logs => self.write_logs_event(),
            GeneratorProfile::Benchmark => self.write_benchmark_event(),
        }
    }

    fn write_logs_event(&mut self) {
        let i = self.counter as usize;
        let level = LEVELS[i % LEVELS.len()];
        let path = PATHS[i % PATHS.len()];
        let method = METHODS[i % METHODS.len()];
        let service = SERVICES[i % SERVICES.len()];
        let id = 10000 + (i.wrapping_mul(7)) % 90000;
        let dur = 1 + (i.wrapping_mul(13)) % 500;
        let rid = self.counter.wrapping_mul(0x517c_c1b7_2722_0a95);
        let status = match i % 20 {
            0 => 500,
            1 | 2 => 404,
            3 => 429,
            _ => 200,
        };

        // Vary timestamps: cycle through hours/minutes/seconds for diversity.
        let hour = i % 24;
        let min = (i / 24) % 60;
        let sec = (i / 1440) % 60;
        let msec = i % 1000;

        match self.config.complexity {
            GeneratorComplexity::Simple => {
                let _ = write!(
                    self.buf,
                    r#"{{"timestamp":"2024-01-15T{hour:02}:{min:02}:{sec:02}.{msec:03}Z","level":"{level}","message":"{method} {path}/{id} {status}","duration_ms":{dur},"request_id":"{rid:016x}","service":"{service}","status":{status}}}"#,
                );
            }
            GeneratorComplexity::Complex => {
                let bytes_in = 128 + (i.wrapping_mul(17)) % 8192;
                let bytes_out = 64 + (i.wrapping_mul(31)) % 4096;
                // Occasionally add nested objects and arrays to exercise the
                // scanner and schema inference more thoroughly.
                if i % 5 == 0 {
                    // Nested: headers object + tags array
                    let _ = write!(
                        self.buf,
                        r#"{{"timestamp":"2024-01-15T{hour:02}:{min:02}:{sec:02}.{msec:03}Z","level":"{level}","message":"{method} {path}/{id} {status}","duration_ms":{dur},"request_id":"{rid:016x}","service":"{service}","status":{status},"bytes_in":{bytes_in},"bytes_out":{bytes_out},"headers":{{"content-type":"application/json","x-request-id":"{rid:016x}"}},"tags":["web","{service}","{level}"]}}"#,
                    );
                } else if i % 7 == 0 {
                    // Nested: upstream array of objects
                    let upstream_ms = 1 + (i.wrapping_mul(19)) % 200;
                    let _ = write!(
                        self.buf,
                        r#"{{"timestamp":"2024-01-15T{hour:02}:{min:02}:{sec:02}.{msec:03}Z","level":"{level}","message":"{method} {path}/{id} {status}","duration_ms":{dur},"request_id":"{rid:016x}","service":"{service}","status":{status},"bytes_in":{bytes_in},"bytes_out":{bytes_out},"upstream":[{{"host":"10.0.0.1","latency_ms":{upstream_ms}}},{{"host":"10.0.0.2","latency_ms":{dur}}}]}}"#,
                    );
                } else {
                    let _ = write!(
                        self.buf,
                        r#"{{"timestamp":"2024-01-15T{hour:02}:{min:02}:{sec:02}.{msec:03}Z","level":"{level}","message":"{method} {path}/{id} {status}","duration_ms":{dur},"request_id":"{rid:016x}","service":"{service}","status":{status},"bytes_in":{bytes_in},"bytes_out":{bytes_out}}}"#,
                    );
                }
            }
        }
    }

    fn write_benchmark_event(&mut self) {
        let i = self.counter as usize;
        let seq = self.counter + 1;
        let level = LEVELS[i % LEVELS.len()];
        let status = if level == "ERROR" { 500 } else { 200 };
        let duration_ms = (i % 250) + 1;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let emit_ts_unix_nano = now.as_nanos();
        let secs = now.as_secs();
        let nanos = now.subsec_nanos();

        self.buf.push(b'{');
        let mut first = true;
        if let Some(benchmark_id) = &self.benchmark_fields.benchmark_id {
            write_json_string_field(&mut self.buf, "benchmark_id", benchmark_id, &mut first);
        }
        write_json_string_field(
            &mut self.buf,
            "pod_name",
            &self.benchmark_fields.pod_name,
            &mut first,
        );
        write_json_string_field(
            &mut self.buf,
            "stream_id",
            &self.benchmark_fields.stream_id,
            &mut first,
        );

        if !first {
            self.buf.push(b',');
        }
        first = false;
        write_json_key(&mut self.buf, "event_id");
        self.buf.push(b':');
        self.buf.push(b'"');
        write_json_escaped_string_contents(&mut self.buf, &self.benchmark_fields.stream_id);
        let _ = write!(&mut self.buf, ":{seq:08}");
        self.buf.push(b'"');

        write_json_u64_field(&mut self.buf, "seq", seq, &mut first);
        write_json_u128_field(
            &mut self.buf,
            "emit_ts_unix_nano",
            emit_ts_unix_nano,
            &mut first,
        );

        if !first {
            self.buf.push(b',');
        }
        first = false;
        write_json_key(&mut self.buf, "timestamp");
        self.buf.push(b':');
        self.buf.push(b'"');
        let _ = write_rfc3339_like_utc(&mut self.buf, secs, nanos);
        self.buf.push(b'"');

        write_json_string_field(&mut self.buf, "level", level, &mut first);

        if !first {
            self.buf.push(b',');
        }
        first = false;
        write_json_key(&mut self.buf, "message");
        self.buf.push(b':');
        self.buf.push(b'"');
        let _ = write!(&mut self.buf, "bench event {seq}");
        self.buf.push(b'"');

        write_json_u64_field(&mut self.buf, "status", status, &mut first);
        write_json_u64_field(&mut self.buf, "duration_ms", duration_ms as u64, &mut first);
        write_json_string_field(
            &mut self.buf,
            "service",
            &self.benchmark_fields.service,
            &mut first,
        );
        self.buf.push(b'}');
    }
}

impl InputSource for GeneratorInput {
    fn poll(&mut self) -> io::Result<Vec<InputEvent>> {
        if self.done {
            return Ok(vec![]);
        }

        // Rate limiting: if events_per_sec > 0, return empty if called too
        // soon rather than blocking the thread. The caller drives the poll
        // loop and can decide how to wait.
        if self.config.events_per_sec > 0 {
            let target_interval = std::time::Duration::from_secs_f64(
                self.config.batch_size as f64 / self.config.events_per_sec as f64,
            );
            let elapsed = self.last_batch.elapsed();
            if elapsed < target_interval {
                return Ok(vec![]);
            }
        }

        self.last_batch = std::time::Instant::now();
        self.generate_batch();

        if self.buf.is_empty() {
            return Ok(vec![]);
        }

        // Swap buffers to preserve capacity (avoid realloc every batch).
        let mut out = Vec::with_capacity(self.config.batch_size * 512);
        std::mem::swap(&mut self.buf, &mut out);
        Ok(vec![InputEvent::Data {
            bytes: out,
            source_id: None,
        }])
    }

    fn name(&self) -> &str {
        &self.name
    }
}

fn write_json_string_field(out: &mut Vec<u8>, key: &str, value: &str, first: &mut bool) {
    if !*first {
        out.push(b',');
    }
    *first = false;
    write_json_key(out, key);
    out.push(b':');
    write_json_quoted_string(out, value);
}

fn write_json_u64_field(out: &mut Vec<u8>, key: &str, value: u64, first: &mut bool) {
    if !*first {
        out.push(b',');
    }
    *first = false;
    write_json_key(out, key);
    let _ = write!(out, ":{value}");
}

fn write_json_u128_field(out: &mut Vec<u8>, key: &str, value: u128, first: &mut bool) {
    if !*first {
        out.push(b',');
    }
    *first = false;
    write_json_key(out, key);
    let _ = write!(out, ":{value}");
}

fn write_json_key(out: &mut Vec<u8>, key: &str) {
    write_json_quoted_string(out, key);
}

fn write_json_quoted_string(out: &mut Vec<u8>, value: &str) {
    out.push(b'"');
    write_json_escaped_string_contents(out, value);
    out.push(b'"');
}

fn write_json_escaped_string_contents(out: &mut Vec<u8>, value: &str) {
    for ch in value.chars() {
        match ch {
            '"' => out.extend_from_slice(br#"\""#),
            '\\' => out.extend_from_slice(br#"\\"#),
            '\n' => out.extend_from_slice(br#"\n"#),
            '\r' => out.extend_from_slice(br#"\r"#),
            '\t' => out.extend_from_slice(br#"\t"#),
            c if c <= '\u{1F}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
}

fn write_rfc3339_like_utc(out: &mut Vec<u8>, secs: u64, nanos: u32) -> io::Result<()> {
    // Fast, dependency-light UTC formatting good enough for synthetic rows.
    let total_days = secs / 86_400;
    let seconds_of_day = secs % 86_400;
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    let z = total_days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    write!(
        out,
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:09}Z",
        nanos
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_valid_json_lines() {
        let mut input = GeneratorInput::new(
            "test",
            GeneratorConfig {
                batch_size: 20,
                total_events: 20,
                ..Default::default()
            },
        );

        let events = input.poll().unwrap();
        assert_eq!(events.len(), 1);

        if let InputEvent::Data { bytes, .. } = &events[0] {
            let text = String::from_utf8_lossy(bytes);
            let lines: Vec<&str> = text.trim().lines().collect();
            assert_eq!(lines.len(), 20);
            // Every line must parse as valid JSON.
            for (i, line) in lines.iter().enumerate() {
                assert!(
                    serde_json::from_str::<serde_json::Value>(line).is_ok(),
                    "line {i} is not valid JSON: {line}"
                );
            }
            assert!(lines[0].contains("\"level\":\"INFO\""));
            assert!(lines[0].contains("\"service\":"));
        } else {
            panic!("expected Data event");
        }

        // Should be done after total_events.
        let events = input.poll().unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn complex_generates_valid_json_lines() {
        let mut input = GeneratorInput::new(
            "test-complex",
            GeneratorConfig {
                batch_size: 50,
                total_events: 50,
                complexity: GeneratorComplexity::Complex,
                ..Default::default()
            },
        );

        let events = input.poll().unwrap();
        assert_eq!(events.len(), 1);

        if let InputEvent::Data { bytes, .. } = &events[0] {
            let text = String::from_utf8_lossy(bytes);
            let lines: Vec<&str> = text.trim().lines().collect();
            assert_eq!(lines.len(), 50);
            let mut saw_nested = false;
            for (i, line) in lines.iter().enumerate() {
                let val: serde_json::Value = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("line {i} invalid JSON: {e}\n{line}"));
                if val.get("headers").is_some() || val.get("upstream").is_some() {
                    saw_nested = true;
                }
            }
            assert!(saw_nested, "complex mode should produce nested objects");
        } else {
            panic!("expected Data event");
        }
    }

    #[test]
    fn rate_limited_returns_empty_when_called_too_soon() {
        let mut input = GeneratorInput::new(
            "test",
            GeneratorConfig {
                batch_size: 10,
                events_per_sec: 1, // 1 event/sec => ~10s per batch of 10
                total_events: 0,
                ..Default::default()
            },
        );

        // First call succeeds.
        let events = input.poll().unwrap();
        assert_eq!(events.len(), 1);

        // Immediate second call should return empty (not block).
        let events = input.poll().unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn unlimited_keeps_going() {
        let mut input = GeneratorInput::new(
            "test",
            GeneratorConfig {
                batch_size: 100,
                total_events: 0, // infinite
                ..Default::default()
            },
        );

        for _ in 0..5 {
            let events = input.poll().unwrap();
            assert_eq!(events.len(), 1);
        }
    }

    #[test]
    fn timestamps_vary_across_events() {
        let mut input = GeneratorInput::new(
            "test",
            GeneratorConfig {
                batch_size: 50,
                total_events: 50,
                ..Default::default()
            },
        );

        let events = input.poll().unwrap();
        if let InputEvent::Data { bytes, .. } = &events[0] {
            let text = String::from_utf8_lossy(bytes);
            let lines: Vec<&str> = text.trim().lines().collect();
            let ts0 = lines[0]
                .find("\"timestamp\":")
                .map(|p| &lines[0][p..p + 50]);
            let ts1 = lines[1]
                .find("\"timestamp\":")
                .map(|p| &lines[1][p..p + 50]);
            // Adjacent lines should have different timestamps because the
            // hour component changes with (i % 24).
            assert_ne!(ts0, ts1, "timestamps should vary between events");
        }
    }

    #[test]
    fn proptest_generated_json_always_valid() {
        // Inline proptest runner: validate JSON for a range of counter offsets
        // by skipping past initial batches to reach different counter values.
        use proptest::prelude::*;

        proptest!(|(offset in 0u64..1000)| {
            // We generate (offset + 1) events and check the last one.
            let total = offset + 1;
            let mut generator = GeneratorInput::new(
                "test",
                GeneratorConfig {
                    batch_size: total as usize,
                    total_events: total,
                    ..Default::default()
                },
            );
            let events = generator.poll().unwrap();
            assert_eq!(events.len(), 1, "poll() must produce exactly one Data event (offset={offset})");
            match &events[0] {
                InputEvent::Data { bytes, .. } => {
                    assert!(!bytes.is_empty(), "generator produced empty data (offset={offset})");
                    let text = String::from_utf8(bytes.clone()).unwrap();
                    let line_count = text.trim().lines().count();
                    assert!(line_count >= 1, "expected at least 1 JSON line, got 0 (offset={offset})");
                    for (i, line) in text.trim().lines().enumerate() {
                        serde_json::from_str::<serde_json::Value>(line)
                            .unwrap_or_else(|e| panic!("invalid JSON at event {i} (offset={offset}): {e}\n{line}"));
                    }
                }
                _ => panic!("unexpected event variant"),
            }
        });
    }

    #[test]
    fn proptest_complex_json_always_valid() {
        use proptest::prelude::*;

        proptest!(|(offset in 0u64..500)| {
            let total = offset + 1;
            let mut generator = GeneratorInput::new(
                "test",
                GeneratorConfig {
                    batch_size: total as usize,
                    total_events: total,
                    complexity: GeneratorComplexity::Complex,
                    ..Default::default()
                },
            );
            let events = generator.poll().unwrap();
            assert_eq!(events.len(), 1, "poll() must produce exactly one Data event (offset={offset})");
            match &events[0] {
                InputEvent::Data { bytes, .. } => {
                    assert!(!bytes.is_empty(), "generator produced empty data (offset={offset})");
                    let text = String::from_utf8(bytes.clone()).unwrap();
                    let line_count = text.trim().lines().count();
                    assert!(line_count >= 1, "expected at least 1 JSON line, got 0 (offset={offset})");
                    for (i, line) in text.trim().lines().enumerate() {
                        serde_json::from_str::<serde_json::Value>(line)
                            .unwrap_or_else(|e| panic!("invalid JSON at event {i} (offset={offset}): {e}\n{line}"));
                    }
                }
                _ => panic!("unexpected event variant"),
            }
        });
    }

    #[test]
    fn events_generated_counter() {
        let mut input = GeneratorInput::new(
            "test",
            GeneratorConfig {
                batch_size: 10,
                total_events: 25,
                ..Default::default()
            },
        );
        assert_eq!(input.events_generated(), 0);
        let _ = input.poll().unwrap();
        assert_eq!(input.events_generated(), 10);
        let _ = input.poll().unwrap();
        assert_eq!(input.events_generated(), 20);
        let _ = input.poll().unwrap();
        assert_eq!(input.events_generated(), 25);
    }

    #[test]
    fn generator_respects_total_events() {
        let mut input = GeneratorInput::new(
            "test",
            GeneratorConfig {
                batch_size: 7, // not a divisor of 50 — exercises partial-batch logic
                total_events: 50,
                ..Default::default()
            },
        );

        let mut total_lines = 0u64;
        loop {
            let events = input.poll().unwrap();
            if events.is_empty() {
                break;
            }
            for event in &events {
                if let InputEvent::Data { bytes, .. } = event {
                    let text = String::from_utf8_lossy(bytes);
                    total_lines += text.trim().lines().count() as u64;
                }
            }
        }

        assert_eq!(
            total_lines, 50,
            "expected exactly 50 events, got {total_lines}"
        );
        assert_eq!(input.events_generated(), 50);

        // Subsequent polls must return empty.
        let events = input.poll().unwrap();
        assert!(events.is_empty(), "poll after completion must be empty");
    }

    #[test]
    fn benchmark_profile_emits_stable_identity_fields() {
        let mut input = GeneratorInput::new(
            "bench",
            GeneratorConfig {
                batch_size: 3,
                total_events: 3,
                profile: GeneratorProfile::Benchmark,
                benchmark_id: Some("run-123".to_string()),
                pod_name: Some("emitter-0".to_string()),
                stream_id: Some("emitter-0".to_string()),
                service: Some("bench-emitter".to_string()),
                ..Default::default()
            },
        );

        let events = input.poll().unwrap();
        assert_eq!(events.len(), 1);
        let InputEvent::Data { bytes, .. } = &events[0] else {
            panic!("expected Data event");
        };
        let text = String::from_utf8_lossy(bytes);
        let lines: Vec<&str> = text.trim().lines().collect();
        assert_eq!(lines.len(), 3);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["benchmark_id"], "run-123");
        assert_eq!(first["pod_name"], "emitter-0");
        assert_eq!(first["stream_id"], "emitter-0");
        assert_eq!(first["event_id"], "emitter-0:00000001");
        assert_eq!(first["seq"], 1);
        assert_eq!(first["service"], "bench-emitter");
        assert!(first.get("emit_ts_unix_nano").is_some());
        assert!(first.get("timestamp").is_some());
    }

    #[test]
    fn benchmark_profile_defaults_stream_identity_from_input_name() {
        let mut input = GeneratorInput::new(
            "bench-input",
            GeneratorConfig {
                batch_size: 1,
                total_events: 1,
                profile: GeneratorProfile::Benchmark,
                ..Default::default()
            },
        );

        let events = input.poll().unwrap();
        let InputEvent::Data { bytes, .. } = &events[0] else {
            panic!("expected Data event");
        };
        let row: serde_json::Value =
            serde_json::from_slice(bytes.split(|b| *b == b'\n').next().unwrap()).unwrap();
        assert_eq!(row["pod_name"], "bench-input");
        assert_eq!(row["stream_id"], "bench-input");
        assert_eq!(row["event_id"], "bench-input:00000001");
    }
}
