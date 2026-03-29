//! Output sink trait and implementations for serializing Arrow RecordBatches
//! to various formats: stdout JSON/text, JSON lines over HTTP, OTLP protobuf.

use std::io::{self, Write};

use arrow::array::{Array, AsArray};
use arrow::datatypes::DataType;
use arrow::record_batch::RecordBatch;

use crate::compress::ChunkCompressor;
use crate::otlp::{
    bytes_field_size, encode_bytes_field, encode_fixed64, encode_tag, encode_varint,
    encode_varint_field, parse_severity, parse_timestamp_nanos, varint_len, Severity,
};

// ---------------------------------------------------------------------------
// Trait + metadata
// ---------------------------------------------------------------------------

/// Metadata about the batch for output serialization.
pub struct BatchMetadata {
    /// Resource attributes (k8s pod name, namespace, etc.)
    pub resource_attrs: Vec<(String, String)>,
    /// Observed timestamp in nanoseconds.
    pub observed_time_ns: u64,
}

/// Every output implements this trait.
pub trait OutputSink: Send {
    /// Serialize and send a batch.
    fn send_batch(&mut self, batch: &RecordBatch, metadata: &BatchMetadata) -> io::Result<()>;
    /// Flush any buffered data.
    fn flush(&mut self) -> io::Result<()>;
    /// Output name (from config).
    fn name(&self) -> &str;
}

// ---------------------------------------------------------------------------
// Column naming helpers
// ---------------------------------------------------------------------------

/// Parse a typed column name into (field_name, type_suffix).
///
/// "duration_ms_int" -> ("duration_ms", "int")
/// "level_str"       -> ("level", "str")
/// "_raw"            -> ("_raw", "")
pub fn parse_column_name(col_name: &str) -> (&str, &str) {
    if let Some(pos) = col_name.rfind('_') {
        let suffix = &col_name[pos + 1..];
        if suffix == "str" || suffix == "int" || suffix == "float" {
            return (&col_name[..pos], suffix);
        }
    }
    (col_name, "")
}

// ---------------------------------------------------------------------------
// JSON serialization helpers (shared by StdoutSink and JsonLinesSink)
// ---------------------------------------------------------------------------

/// Describes one output column: its index in the RecordBatch, the field name
/// (with type suffix stripped), the type suffix, and the Arrow data type.
struct ColInfo {
    idx: usize,
    field_name: String,
    type_suffix: String,
}

/// Build a de-duplicated ordered list of columns for JSON output.
/// When the same field_name appears multiple times (e.g. status_int and
/// status_str), prefer int > float > str.
fn build_col_infos(batch: &RecordBatch) -> Vec<ColInfo> {
    let schema = batch.schema();
    let mut infos: Vec<ColInfo> = Vec::new();
    // Collect all columns.
    for (idx, field) in schema.fields().iter().enumerate() {
        let name = field.name().as_str();
        let (field_name, type_suffix) = parse_column_name(name);
        infos.push(ColInfo {
            idx,
            field_name: field_name.to_string(),
            type_suffix: type_suffix.to_string(),
        });
    }
    // De-duplicate: for each field_name keep the best-typed column.
    // Priority: int > float > str > untyped
    fn type_priority(suffix: &str) -> u8 {
        match suffix {
            "int" => 3,
            "float" => 2,
            "str" => 1,
            _ => 0,
        }
    }
    // Use a stable sort + dedup to keep the highest-priority for each name.
    infos.sort_by(|a, b| {
        a.field_name
            .cmp(&b.field_name)
            .then_with(|| type_priority(&b.type_suffix).cmp(&type_priority(&a.type_suffix)))
    });
    infos.dedup_by(|a, b| a.field_name == b.field_name);
    // Re-sort by original column index to maintain stable output order.
    infos.sort_by_key(|c| c.idx);
    infos
}

/// Write a single row as a JSON object into `out`.
fn write_row_json(batch: &RecordBatch, row: usize, cols: &[ColInfo], out: &mut Vec<u8>) {
    out.push(b'{');
    let mut first = true;
    for col in cols {
        let arr = batch.column(col.idx);
        if arr.is_null(row) {
            continue;
        }
        if !first {
            out.push(b',');
        }
        first = false;
        // Key
        out.push(b'"');
        out.extend_from_slice(col.field_name.as_bytes());
        out.push(b'"');
        out.push(b':');
        // Value — type-aware
        match col.type_suffix.as_str() {
            "int" => {
                let arr = arr.as_primitive::<arrow::datatypes::Int64Type>();
                let v = arr.value(row);
                // Write integer directly; no quotes in JSON.
                let _ = io::Write::write_fmt(out, format_args!("{}", v));
            }
            "float" => {
                let arr = arr.as_primitive::<arrow::datatypes::Float64Type>();
                let v = arr.value(row);
                let _ = io::Write::write_fmt(out, format_args!("{}", v));
            }
            _ => {
                // str or untyped — treat as string
                let arr = arr.as_string::<i32>();
                let v = arr.value(row);
                out.push(b'"');
                // Minimal JSON escape
                for &b in v.as_bytes() {
                    match b {
                        b'"' => out.extend_from_slice(b"\\\""),
                        b'\\' => out.extend_from_slice(b"\\\\"),
                        b'\n' => out.extend_from_slice(b"\\n"),
                        b'\r' => out.extend_from_slice(b"\\r"),
                        b'\t' => out.extend_from_slice(b"\\t"),
                        _ => out.push(b),
                    }
                }
                out.push(b'"');
            }
        }
    }
    out.push(b'}');
}

// ---------------------------------------------------------------------------
// StdoutSink
// ---------------------------------------------------------------------------

/// Output format for StdoutSink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdoutFormat {
    Json,
    Text,
}

/// Writes log records to stdout, one per line.
pub struct StdoutSink {
    name: String,
    format: StdoutFormat,
    buf: Vec<u8>,
}

impl StdoutSink {
    pub fn new(name: String, format: StdoutFormat) -> Self {
        StdoutSink {
            name,
            format,
            buf: Vec::with_capacity(8192),
        }
    }

    /// Write into an arbitrary `Write` destination (useful for testing).
    pub fn write_batch_to<W: Write>(
        &mut self,
        batch: &RecordBatch,
        _metadata: &BatchMetadata,
        dest: &mut W,
    ) -> io::Result<()> {
        let num_rows = batch.num_rows();
        if num_rows == 0 {
            return Ok(());
        }

        match self.format {
            StdoutFormat::Text => {
                // Try to find _raw column.
                let raw_idx = batch
                    .schema()
                    .fields()
                    .iter()
                    .position(|f| f.name() == "_raw");
                if let Some(idx) = raw_idx {
                    let arr = batch.column(idx).as_string::<i32>();
                    for row in 0..num_rows {
                        if !arr.is_null(row) {
                            dest.write_all(arr.value(row).as_bytes())?;
                            dest.write_all(b"\n")?;
                        }
                    }
                } else {
                    // Fall back to JSON
                    let cols = build_col_infos(batch);
                    for row in 0..num_rows {
                        self.buf.clear();
                        write_row_json(batch, row, &cols, &mut self.buf);
                        self.buf.push(b'\n');
                        dest.write_all(&self.buf)?;
                    }
                }
            }
            StdoutFormat::Json => {
                let cols = build_col_infos(batch);
                for row in 0..num_rows {
                    self.buf.clear();
                    write_row_json(batch, row, &cols, &mut self.buf);
                    self.buf.push(b'\n');
                    dest.write_all(&self.buf)?;
                }
            }
        }
        Ok(())
    }
}

impl OutputSink for StdoutSink {
    fn send_batch(&mut self, batch: &RecordBatch, metadata: &BatchMetadata) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        self.write_batch_to(batch, metadata, &mut stdout)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }

    fn name(&self) -> &str {
        &self.name
    }
}

// ---------------------------------------------------------------------------
// JsonLinesSink
// ---------------------------------------------------------------------------

/// Writes newline-delimited JSON and POSTs over HTTP.
pub struct JsonLinesSink {
    name: String,
    url: String,
    headers: Vec<(String, String)>,
    batch_buf: Vec<u8>,
}

impl JsonLinesSink {
    pub fn new(name: String, url: String, headers: Vec<(String, String)>) -> Self {
        JsonLinesSink {
            name,
            url,
            headers,
            batch_buf: Vec::with_capacity(64 * 1024),
        }
    }

    /// Check whether the batch is "raw passthrough eligible": has a `_raw` column
    /// and every other column is null for every row (no transforms modified fields).
    fn is_raw_passthrough(batch: &RecordBatch) -> bool {
        let schema = batch.schema();
        let has_raw = schema.fields().iter().any(|f| f.name() == "_raw");
        if !has_raw {
            return false;
        }
        // Simple heuristic: if the only non-null column is _raw, passthrough.
        for field in schema.fields().iter() {
            if field.name() == "_raw" {
                continue;
            }
            let idx = schema.index_of(field.name()).unwrap();
            if batch.column(idx).null_count() < batch.num_rows() {
                return false;
            }
        }
        true
    }

    /// Serialize the batch into `self.batch_buf` as newline-delimited JSON.
    pub fn serialize_batch(&mut self, batch: &RecordBatch) {
        self.batch_buf.clear();
        let num_rows = batch.num_rows();
        if num_rows == 0 {
            return;
        }

        if Self::is_raw_passthrough(batch) {
            // Fast path: memcpy _raw values directly.
            let idx = batch
                .schema()
                .index_of("_raw")
                .expect("_raw column missing");
            let arr = batch.column(idx).as_string::<i32>();
            for row in 0..num_rows {
                if !arr.is_null(row) {
                    self.batch_buf.extend_from_slice(arr.value(row).as_bytes());
                    self.batch_buf.push(b'\n');
                }
            }
        } else {
            let cols = build_col_infos(batch);
            for row in 0..num_rows {
                write_row_json(batch, row, &cols, &mut self.batch_buf);
                self.batch_buf.push(b'\n');
            }
        }
    }
}

impl OutputSink for JsonLinesSink {
    fn send_batch(&mut self, batch: &RecordBatch, _metadata: &BatchMetadata) -> io::Result<()> {
        self.serialize_batch(batch);
        if self.batch_buf.is_empty() {
            return Ok(());
        }

        let mut req = ureq::post(&self.url);
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req = req.header("Content-Type", "application/x-ndjson");

        req.send(&self.batch_buf)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

// ---------------------------------------------------------------------------
// OtlpSink
// ---------------------------------------------------------------------------

/// OTLP transport protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpProtocol {
    Grpc,
    Http,
}

/// Compression algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Zstd,
    Gzip,
    None,
}

/// Sends OTLP protobuf LogRecords over gRPC or HTTP.
pub struct OtlpSink {
    name: String,
    endpoint: String,
    protocol: OtlpProtocol,
    compression: Compression,
    encoder_buf: Vec<u8>,
    compress_buf: Vec<u8>,
    compressor: Option<ChunkCompressor>,
}

impl OtlpSink {
    pub fn new(
        name: String,
        endpoint: String,
        protocol: OtlpProtocol,
        compression: Compression,
    ) -> Self {
        let compressor = match compression {
            Compression::Zstd => Some(ChunkCompressor::new(1)),
            _ => None,
        };
        OtlpSink {
            name,
            endpoint,
            protocol,
            compression,
            encoder_buf: Vec::with_capacity(64 * 1024),
            compress_buf: Vec::with_capacity(64 * 1024),
            compressor,
        }
    }

    /// Encode a full ExportLogsServiceRequest from a RecordBatch.
    /// Returns the raw protobuf bytes in `self.encoder_buf`.
    pub fn encode_batch(&mut self, batch: &RecordBatch, metadata: &BatchMetadata) {
        self.encoder_buf.clear();
        let num_rows = batch.num_rows();
        if num_rows == 0 {
            return;
        }

        // Phase 1: encode all LogRecords into a temp buffer.
        let mut records_buf: Vec<u8> = Vec::with_capacity(num_rows * 128);
        let mut record_ranges: Vec<(usize, usize)> = Vec::with_capacity(num_rows);

        for row in 0..num_rows {
            let start = records_buf.len();
            encode_row_as_log_record(batch, row, metadata, &mut records_buf);
            record_ranges.push((start, records_buf.len()));
        }

        // Phase 2: compute sizes bottom-up.
        // ScopeLogs inner = repeated field 2 (LogRecord) entries
        let mut scope_logs_inner_size = 0usize;
        for &(start, end) in &record_ranges {
            let record_len = end - start;
            // tag for field 2 wire type 2 + varint length + payload
            scope_logs_inner_size +=
                varint_len(((2u64) << 3) | 2) + varint_len(record_len as u64) + record_len;
        }

        // ResourceLogs inner = resource attributes (field 1) + scope_logs (field 2)
        let mut resource_inner_size = bytes_field_size(2, scope_logs_inner_size);

        // Encode resource attributes as Resource message (field 1 of ResourceLogs)
        let mut resource_msg: Vec<u8> = Vec::new();
        if !metadata.resource_attrs.is_empty() {
            for (k, v) in &metadata.resource_attrs {
                encode_key_value_string(&mut resource_msg, k.as_bytes(), v.as_bytes());
            }
        }
        if !resource_msg.is_empty() {
            // Resource message field 1 (attributes) — we wrote KeyValues directly.
            // Wrap in Resource message (field 1 of ResourceLogs).
            resource_inner_size += bytes_field_size(1, resource_msg.len());
        }

        let request_size = bytes_field_size(1, resource_inner_size);

        // Phase 3: write the final protobuf.
        self.encoder_buf.reserve(request_size + 16);

        // ExportLogsServiceRequest.resource_logs (field 1)
        encode_tag(&mut self.encoder_buf, 1, 2);
        encode_varint(&mut self.encoder_buf, resource_inner_size as u64);

        // Resource (field 1 of ResourceLogs)
        if !resource_msg.is_empty() {
            encode_bytes_field(&mut self.encoder_buf, 1, &resource_msg);
        }

        // ScopeLogs (field 2 of ResourceLogs)
        encode_tag(&mut self.encoder_buf, 2, 2);
        encode_varint(&mut self.encoder_buf, scope_logs_inner_size as u64);

        // LogRecords (field 2 of ScopeLogs, repeated)
        for &(start, end) in &record_ranges {
            encode_bytes_field(&mut self.encoder_buf, 2, &records_buf[start..end]);
        }
    }
}

impl OutputSink for OtlpSink {
    fn send_batch(&mut self, batch: &RecordBatch, metadata: &BatchMetadata) -> io::Result<()> {
        self.encode_batch(batch, metadata);
        if self.encoder_buf.is_empty() {
            return Ok(());
        }

        let payload: &[u8] = match self.compression {
            Compression::Zstd => {
                if let Some(ref mut compressor) = self.compressor {
                    let chunk = compressor.compress(&self.encoder_buf)?;
                    self.compress_buf.clear();
                    self.compress_buf.extend_from_slice(&chunk.data);
                    &self.compress_buf
                } else {
                    &self.encoder_buf
                }
            }
            Compression::Gzip | Compression::None => &self.encoder_buf,
        };

        let content_type = match self.protocol {
            OtlpProtocol::Grpc => "application/grpc",
            OtlpProtocol::Http => "application/x-protobuf",
        };

        let mut req = ureq::post(&self.endpoint);
        req = req.header("Content-Type", content_type);
        if self.compression == Compression::Zstd {
            req = req.header("Content-Encoding", "zstd");
        }

        req.send(payload)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// Encode a single RecordBatch row as an OTLP LogRecord.
fn encode_row_as_log_record(
    batch: &RecordBatch,
    row: usize,
    metadata: &BatchMetadata,
    buf: &mut Vec<u8>,
) {
    let schema = batch.schema();

    // --- Find special columns ---
    let mut timestamp_ns: u64 = 0;
    let mut severity_num = Severity::Unspecified;
    let mut severity_text: &[u8] = b"";
    let mut body: Option<&str> = None;
    let mut body_col_idx: Option<usize> = None;
    let mut timestamp_col_idx: Option<usize> = None;
    let mut level_col_idx: Option<usize> = None;

    for (idx, field) in schema.fields().iter().enumerate() {
        let col_name = field.name().as_str();
        let (field_name, _type_suffix) = parse_column_name(col_name);

        match field_name {
            "timestamp" | "time" | "ts" => {
                if timestamp_col_idx.is_none() && !batch.column(idx).is_null(row) {
                    timestamp_col_idx = Some(idx);
                    if let DataType::Utf8 = field.data_type() {
                        let arr = batch.column(idx).as_string::<i32>();
                        let val = arr.value(row);
                        timestamp_ns = parse_timestamp_nanos(val.as_bytes());
                    }
                }
            }
            "level" | "severity" | "log_level" | "loglevel" | "lvl" => {
                if level_col_idx.is_none() && !batch.column(idx).is_null(row) {
                    level_col_idx = Some(idx);
                    if let DataType::Utf8 = field.data_type() {
                        let arr = batch.column(idx).as_string::<i32>();
                        let val = arr.value(row);
                        let (sev, text) = parse_severity(val.as_bytes());
                        severity_num = sev;
                        severity_text = text;
                    }
                }
            }
            "message" | "msg" | "_msg" | "body" => {
                if body_col_idx.is_none() && !batch.column(idx).is_null(row) {
                    body_col_idx = Some(idx);
                    if let DataType::Utf8 = field.data_type() {
                        let arr = batch.column(idx).as_string::<i32>();
                        body = Some(arr.value(row));
                    }
                }
            }
            "_raw" => {
                // Fallback body source
                if body_col_idx.is_none() && !batch.column(idx).is_null(row) {
                    body_col_idx = Some(idx);
                    let arr = batch.column(idx).as_string::<i32>();
                    body = Some(arr.value(row));
                }
            }
            _ => {}
        }
    }

    let body_bytes = body.unwrap_or("").as_bytes();

    // --- Write protobuf fields ---

    // field 1: time_unix_nano (fixed64)
    if timestamp_ns > 0 {
        encode_fixed64(buf, 1, timestamp_ns);
    }

    // field 2: severity_number (varint)
    if severity_num as u8 > 0 {
        encode_varint_field(buf, 2, severity_num as u64);
    }

    // field 3: severity_text (string)
    if !severity_text.is_empty() {
        encode_bytes_field(buf, 3, severity_text);
    }

    // field 5: body (AnyValue { string_value })
    if !body_bytes.is_empty() {
        let anyvalue_inner_size = bytes_field_size(1, body_bytes.len());
        encode_tag(buf, 5, 2);
        encode_varint(buf, anyvalue_inner_size as u64);
        encode_bytes_field(buf, 1, body_bytes);
    }

    // field 6: attributes — all remaining columns
    for (idx, field) in schema.fields().iter().enumerate() {
        if Some(idx) == timestamp_col_idx
            || Some(idx) == level_col_idx
            || Some(idx) == body_col_idx
        {
            continue;
        }
        let col_name = field.name().as_str();
        let (field_name, type_suffix) = parse_column_name(col_name);
        if field_name == "_raw" {
            continue;
        }
        if batch.column(idx).is_null(row) {
            continue;
        }

        match type_suffix {
            "int" => {
                let arr = batch.column(idx).as_primitive::<arrow::datatypes::Int64Type>();
                let v = arr.value(row);
                encode_key_value_int(buf, field_name.as_bytes(), v);
            }
            "float" => {
                let arr = batch.column(idx).as_primitive::<arrow::datatypes::Float64Type>();
                let v = arr.value(row);
                encode_key_value_double(buf, field_name.as_bytes(), v);
            }
            _ => {
                let arr = batch.column(idx).as_string::<i32>();
                let v = arr.value(row);
                encode_key_value_string(buf, field_name.as_bytes(), v.as_bytes());
            }
        }
    }

    // field 11: observed_time_unix_nano (fixed64)
    encode_fixed64(buf, 11, metadata.observed_time_ns);
}

/// Encode a KeyValue with string AnyValue as an attribute (field 6 of LogRecord).
/// KeyValue: { key (field 1, string), value (field 2, AnyValue { string_value (field 1) }) }
fn encode_key_value_string(buf: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    let anyvalue_inner = bytes_field_size(1, value.len()); // AnyValue.string_value
    let kv_inner = bytes_field_size(1, key.len()) + bytes_field_size(2, anyvalue_inner);
    // LogRecord field 6, wire type 2
    encode_tag(buf, 6, 2);
    encode_varint(buf, kv_inner as u64);
    // KeyValue.key = field 1
    encode_bytes_field(buf, 1, key);
    // KeyValue.value = field 2 (AnyValue)
    encode_tag(buf, 2, 2);
    encode_varint(buf, anyvalue_inner as u64);
    // AnyValue.string_value = field 1
    encode_bytes_field(buf, 1, value);
}

/// Encode a KeyValue with int AnyValue (field 3 of AnyValue = int_value).
fn encode_key_value_int(buf: &mut Vec<u8>, key: &[u8], value: i64) {
    let anyvalue_inner = 1 + varint_len(value as u64); // tag(1 byte) + varint
    let kv_inner = bytes_field_size(1, key.len()) + bytes_field_size(2, anyvalue_inner);
    encode_tag(buf, 6, 2);
    encode_varint(buf, kv_inner as u64);
    encode_bytes_field(buf, 1, key);
    encode_tag(buf, 2, 2);
    encode_varint(buf, anyvalue_inner as u64);
    // AnyValue.int_value = field 3, wire type 0 (varint)
    encode_varint_field(buf, 3, value as u64);
}

/// Encode a KeyValue with double AnyValue (field 4 of AnyValue = double_value).
fn encode_key_value_double(buf: &mut Vec<u8>, key: &[u8], value: f64) {
    let anyvalue_inner = 1 + 8; // tag(1 byte) + fixed64
    let kv_inner = bytes_field_size(1, key.len()) + bytes_field_size(2, anyvalue_inner);
    encode_tag(buf, 6, 2);
    encode_varint(buf, kv_inner as u64);
    encode_bytes_field(buf, 1, key);
    encode_tag(buf, 2, 2);
    encode_varint(buf, anyvalue_inner as u64);
    // AnyValue.double_value = field 4, wire type 1 (64-bit fixed)
    encode_fixed64(buf, 4, value.to_bits());
}

// ---------------------------------------------------------------------------
// FanOut
// ---------------------------------------------------------------------------

/// Multiplexes output to multiple sinks.
pub struct FanOut {
    sinks: Vec<Box<dyn OutputSink>>,
}

impl FanOut {
    pub fn new(sinks: Vec<Box<dyn OutputSink>>) -> Self {
        FanOut { sinks }
    }
}

impl OutputSink for FanOut {
    fn send_batch(&mut self, batch: &RecordBatch, meta: &BatchMetadata) -> io::Result<()> {
        for sink in &mut self.sinks {
            sink.send_batch(batch, meta)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        for sink in &mut self.sinks {
            sink.flush()?;
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "fanout"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test helper: captures all sent batches for assertion.
#[cfg(test)]
pub struct CaptureSink {
    name: String,
    pub batches: Vec<RecordBatch>,
}

#[cfg(test)]
impl CaptureSink {
    pub fn new(name: &str) -> Self {
        CaptureSink {
            name: name.to_string(),
            batches: Vec::new(),
        }
    }
}

#[cfg(test)]
impl OutputSink for CaptureSink {
    fn send_batch(&mut self, batch: &RecordBatch, _metadata: &BatchMetadata) -> io::Result<()> {
        self.batches.push(batch.clone());
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    fn make_test_batch() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("level_str", DataType::Utf8, true),
            Field::new("status_int", DataType::Int64, true),
        ]));
        let level = StringArray::from(vec![Some("ERROR"), Some("INFO")]);
        let status = Int64Array::from(vec![Some(500), Some(200)]);
        RecordBatch::try_new(schema, vec![Arc::new(level), Arc::new(status)]).unwrap()
    }

    fn make_metadata() -> BatchMetadata {
        BatchMetadata {
            resource_attrs: vec![],
            observed_time_ns: 1_700_000_000_000_000_000,
        }
    }

    #[test]
    fn test_parse_column_name() {
        assert_eq!(parse_column_name("status_int"), ("status", "int"));
        assert_eq!(parse_column_name("level_str"), ("level", "str"));
        assert_eq!(parse_column_name("duration_ms_float"), ("duration_ms", "float"));
        assert_eq!(parse_column_name("_raw"), ("_raw", ""));
        assert_eq!(parse_column_name("plain"), ("plain", ""));
    }

    #[test]
    fn test_stdout_json() {
        let batch = make_test_batch();
        let meta = make_metadata();
        let mut sink = StdoutSink::new("test".to_string(), StdoutFormat::Json);
        let mut out: Vec<u8> = Vec::new();
        sink.write_batch_to(&batch, &meta, &mut out).unwrap();

        let output = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = output.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        // First row: level=ERROR, status=500
        assert!(lines[0].contains("\"level\":\"ERROR\""), "got: {}", lines[0]);
        assert!(lines[0].contains("\"status\":500"), "got: {}", lines[0]);
        // Second row: level=INFO, status=200
        assert!(lines[1].contains("\"level\":\"INFO\""), "got: {}", lines[1]);
        assert!(lines[1].contains("\"status\":200"), "got: {}", lines[1]);
    }

    #[test]
    fn test_fanout() {
        // FanOut to two sinks that write to Vec<u8>.
        // We use StdoutSink with write_batch_to to capture output.
        let batch = make_test_batch();
        let meta = make_metadata();

        let mut sink1 = StdoutSink::new("s1".to_string(), StdoutFormat::Json);
        let mut sink2 = StdoutSink::new("s2".to_string(), StdoutFormat::Json);

        let mut out1: Vec<u8> = Vec::new();
        let mut out2: Vec<u8> = Vec::new();

        sink1.write_batch_to(&batch, &meta, &mut out1).unwrap();
        sink2.write_batch_to(&batch, &meta, &mut out2).unwrap();

        // Both should have identical output.
        assert_eq!(out1, out2);
        assert!(!out1.is_empty());

        // Also test FanOut trait dispatch works.
        let fanout_s1 = StdoutSink::new("f1".to_string(), StdoutFormat::Json);
        let fanout_s2 = StdoutSink::new("f2".to_string(), StdoutFormat::Json);
        let mut fanout = FanOut::new(vec![
            Box::new(fanout_s1),
            Box::new(fanout_s2),
        ]);
        // send_batch writes to real stdout, but should not error.
        let result = fanout.send_batch(&batch, &meta);
        assert!(result.is_ok());
    }

    #[test]
    fn test_otlp_encoding() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp_str", DataType::Utf8, true),
            Field::new("level_str", DataType::Utf8, true),
            Field::new("message_str", DataType::Utf8, true),
            Field::new("status_int", DataType::Int64, true),
        ]));
        let ts = StringArray::from(vec![Some("2024-01-15T10:30:00Z")]);
        let level = StringArray::from(vec![Some("ERROR")]);
        let msg = StringArray::from(vec![Some("something broke")]);
        let status = Int64Array::from(vec![Some(500)]);
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(ts), Arc::new(level), Arc::new(msg), Arc::new(status)],
        )
        .unwrap();

        let meta = BatchMetadata {
            resource_attrs: vec![("k8s.pod.name".to_string(), "myapp-abc".to_string())],
            observed_time_ns: 1_700_000_000_000_000_000,
        };

        let mut sink = OtlpSink::new(
            "test-otlp".to_string(),
            "http://localhost:4318".to_string(),
            OtlpProtocol::Http,
            Compression::None,
        );
        sink.encode_batch(&batch, &meta);

        // Should produce non-empty protobuf bytes.
        assert!(!sink.encoder_buf.is_empty());
        // First byte should be tag for field 1 (ResourceLogs), wire type 2 = 0x0A
        assert_eq!(sink.encoder_buf[0], 0x0A);
    }

    #[test]
    fn test_raw_passthrough() {
        // Build a batch with only _raw column (simulating no transforms).
        let schema = Arc::new(Schema::new(vec![
            Field::new("_raw", DataType::Utf8, true),
        ]));
        let raw = StringArray::from(vec![
            Some(r#"{"ts":"2024-01-15","msg":"hello"}"#),
            Some(r#"{"ts":"2024-01-15","msg":"world"}"#),
        ]);
        let batch = RecordBatch::try_new(schema, vec![Arc::new(raw)]).unwrap();

        let mut sink = JsonLinesSink::new(
            "test-jsonl".to_string(),
            "http://localhost:9200".to_string(),
            vec![],
        );
        sink.serialize_batch(&batch);

        let output = String::from_utf8(sink.batch_buf.clone()).unwrap();
        let lines: Vec<&str> = output.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        // Should be the original JSON, not re-serialized.
        assert_eq!(lines[0], r#"{"ts":"2024-01-15","msg":"hello"}"#);
        assert_eq!(lines[1], r#"{"ts":"2024-01-15","msg":"world"}"#);
    }

    #[test]
    fn test_stdout_text_format() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("_raw", DataType::Utf8, true),
            Field::new("level_str", DataType::Utf8, true),
        ]));
        let raw = StringArray::from(vec![Some("original log line")]);
        let level = StringArray::from(vec![Some("INFO")]);
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(raw), Arc::new(level)]).unwrap();
        let meta = make_metadata();

        let mut sink = StdoutSink::new("test".to_string(), StdoutFormat::Text);
        let mut out: Vec<u8> = Vec::new();
        sink.write_batch_to(&batch, &meta, &mut out).unwrap();
        let output = String::from_utf8(out).unwrap();
        assert_eq!(output.trim(), "original log line");
    }

    #[test]
    fn test_type_preference_dedup() {
        // When both status_int and status_str exist, int should win.
        let schema = Arc::new(Schema::new(vec![
            Field::new("status_str", DataType::Utf8, true),
            Field::new("status_int", DataType::Int64, true),
        ]));
        let status_s = StringArray::from(vec![Some("500")]);
        let status_i = Int64Array::from(vec![Some(500)]);
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(status_s), Arc::new(status_i)]).unwrap();
        let meta = make_metadata();

        let mut sink = StdoutSink::new("test".to_string(), StdoutFormat::Json);
        let mut out: Vec<u8> = Vec::new();
        sink.write_batch_to(&batch, &meta, &mut out).unwrap();
        let output = String::from_utf8(out).unwrap();
        // Should have integer 500, not string "500"
        assert!(output.contains("\"status\":500"), "got: {}", output);
        // Should NOT have the string version
        assert!(!output.contains("\"status\":\"500\""), "got: {}", output);
    }

    #[test]
    fn test_float_column_json() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("duration_ms_float", DataType::Float64, true),
        ]));
        let dur = Float64Array::from(vec![Some(3.14)]);
        let batch = RecordBatch::try_new(schema, vec![Arc::new(dur)]).unwrap();
        let meta = make_metadata();

        let mut sink = StdoutSink::new("test".to_string(), StdoutFormat::Json);
        let mut out: Vec<u8> = Vec::new();
        sink.write_batch_to(&batch, &meta, &mut out).unwrap();
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("\"duration_ms\":3.14"), "got: {}", output);
    }
}
