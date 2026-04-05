//! Contract POC: OTLP output sink -> OTLP input receiver loopback.
//!
//! We run the production OTLP output path and the production OTLP HTTP input
//! path together, then assert semantic field preservation on the decoded JSON
//! line emitted by `InputSource::poll`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{BooleanArray, Float64Array, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use logfwd_io::input::{InputEvent, InputSource};
use logfwd_io::otlp_receiver::OtlpReceiverInput;
use logfwd_output::sink::Sink;
use logfwd_output::{BatchMetadata, Compression, OtlpProtocol, OtlpSink};
use logfwd_types::diagnostics::ComponentStats;

fn poll_json_lines(input: &mut dyn InputSource, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();

    while Instant::now() < deadline {
        for event in input.poll().expect("poll receiver") {
            if let InputEvent::Data { bytes, .. } = event {
                buf.extend_from_slice(&bytes);
            }
        }
        if !buf.is_empty() {
            return String::from_utf8(buf).expect("receiver emitted utf8 json lines");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    panic!("timed out waiting for OTLP receiver data");
}

#[tokio::test(flavor = "current_thread")]
async fn otlp_output_to_input_loopback_preserves_semantics_under_zstd() {
    let mut receiver =
        OtlpReceiverInput::new("test-otlp-in", "127.0.0.1:0").expect("start OTLP receiver");
    let endpoint = format!("http://{}/v1/logs", receiver.local_addr());

    let mut sink = OtlpSink::new(
        "test-otlp-out".into(),
        endpoint,
        OtlpProtocol::Http,
        Compression::Zstd,
        vec![],
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build reqwest client"),
        Arc::new(ComponentStats::new()),
    )
    .expect("create OTLP sink");

    let schema = Arc::new(Schema::new(vec![
        Field::new("timestamp", DataType::Utf8, true),
        Field::new("level", DataType::Utf8, true),
        Field::new("message", DataType::Utf8, true),
        Field::new("trace_id", DataType::Utf8, true),
        Field::new("span_id", DataType::Utf8, true),
        Field::new("request_count", DataType::Int64, true),
        Field::new("latency_ratio", DataType::Float64, true),
        Field::new("sampled", DataType::Boolean, true),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(vec!["2024-01-15T10:30:00Z"])),
            Arc::new(StringArray::from(vec!["ERROR"])),
            Arc::new(StringArray::from(vec!["disk full"])),
            Arc::new(StringArray::from(vec!["0102030405060708090a0b0c0d0e0f10"])),
            Arc::new(StringArray::from(vec!["0102030405060708"])),
            Arc::new(Int64Array::from(vec![7])),
            Arc::new(Float64Array::from(vec![3.5])),
            Arc::new(BooleanArray::from(vec![true])),
        ],
    )
    .expect("valid batch");

    let metadata = BatchMetadata {
        resource_attrs: Arc::new(vec![(
            "service.name".to_string(),
            "checkout-api".to_string(),
        )]),
        observed_time_ns: 1_700_000_000_000_000_000,
    };

    sink.send_batch(&batch, &metadata).await.unwrap();

    let lines = poll_json_lines(&mut receiver, Duration::from_secs(2));
    let values: Vec<serde_json::Value> = lines
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid json line"))
        .collect();
    assert_eq!(values.len(), 1, "expected one output JSON line");

    let row = &values[0];
    assert_eq!(
        row.get("level").and_then(serde_json::Value::as_str),
        Some("ERROR")
    );
    assert_eq!(
        row.get("message").and_then(serde_json::Value::as_str),
        Some("disk full")
    );
    assert_eq!(
        row.get("trace_id").and_then(serde_json::Value::as_str),
        Some("0102030405060708090a0b0c0d0e0f10")
    );
    assert_eq!(
        row.get("span_id").and_then(serde_json::Value::as_str),
        Some("0102030405060708")
    );
    assert_eq!(
        row.get("request_count").and_then(serde_json::Value::as_i64),
        Some(7)
    );
    assert_eq!(
        row.get("latency_ratio").and_then(serde_json::Value::as_f64),
        Some(3.5)
    );
    assert_eq!(
        row.get("sampled").and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        row.get("service.name").and_then(serde_json::Value::as_str),
        Some("checkout-api")
    );
}
