// simd_scanner.rs — Chunk-level SIMD JSON-to-Arrow scanner.
//
// Stage 1: Classify the entire NDJSON buffer in one SIMD pass (ChunkIndex).
//          Produces pre-computed bitmasks of quote and string positions.
//          Runs at ~21 GiB/s — essentially free.
//
// Stage 2: Walk the buffer line-by-line. For each line, extract top-level
//          key-value pairs using the pre-computed index. String scanning
//          is O(1) bit-scan instead of per-byte comparison.
//
// No DOM. No tape. No intermediate representation. Direct to Arrow.

use crate::batch_builder::parse_int_fast;
use crate::chunk_classify::ChunkIndex;
use crate::columnar_builder::ColumnarBatchBuilder;
use crate::indexed_builder::IndexedBatchBuilder;
use crate::scanner::ScanConfig;
use arrow::record_batch::RecordBatch;
use memchr::memchr;

// ---------------------------------------------------------------------------
// ScanBuilder trait — shared interface for index-based builders
// ---------------------------------------------------------------------------

/// Trait for builders that support index-based field access.
///
/// Both `IndexedBatchBuilder` and `ColumnarBatchBuilder` implement this,
/// allowing the scan loop to be shared without code duplication.
pub(crate) trait ScanBuilder {
    fn begin_batch(&mut self);
    fn begin_row(&mut self);
    fn end_row(&mut self);
    fn resolve_field(&mut self, key: &[u8]) -> usize;
    fn append_str_by_idx(&mut self, idx: usize, value: &[u8]);
    fn append_int_by_idx(&mut self, idx: usize, value: &[u8]);
    fn append_float_by_idx(&mut self, idx: usize, value: &[u8]);
    fn append_null_by_idx(&mut self, idx: usize);
    fn append_raw(&mut self, line: &[u8]);
    /// Set the input buffer pointer. Only needed for zero-copy builders.
    fn set_buffer(&mut self, _buf: &[u8]) {}
}

impl ScanBuilder for IndexedBatchBuilder {
    #[inline(always)]
    fn begin_batch(&mut self) { self.begin_batch(); }
    #[inline(always)]
    fn begin_row(&mut self) { self.begin_row(); }
    #[inline(always)]
    fn end_row(&mut self) { self.end_row(); }
    #[inline(always)]
    fn resolve_field(&mut self, key: &[u8]) -> usize { self.resolve_field(key) }
    #[inline(always)]
    fn append_str_by_idx(&mut self, idx: usize, value: &[u8]) { self.append_str_by_idx(idx, value); }
    #[inline(always)]
    fn append_int_by_idx(&mut self, idx: usize, value: &[u8]) { self.append_int_by_idx(idx, value); }
    #[inline(always)]
    fn append_float_by_idx(&mut self, idx: usize, value: &[u8]) { self.append_float_by_idx(idx, value); }
    #[inline(always)]
    fn append_null_by_idx(&mut self, idx: usize) { self.append_null_by_idx(idx); }
    #[inline(always)]
    fn append_raw(&mut self, line: &[u8]) { self.append_raw(line); }
}

impl ScanBuilder for ColumnarBatchBuilder {
    #[inline(always)]
    fn begin_batch(&mut self) { self.begin_batch(); }
    #[inline(always)]
    fn begin_row(&mut self) { self.begin_row(); }
    #[inline(always)]
    fn end_row(&mut self) { self.end_row(); }
    #[inline(always)]
    fn resolve_field(&mut self, key: &[u8]) -> usize { self.resolve_field(key) }
    #[inline(always)]
    fn append_str_by_idx(&mut self, idx: usize, value: &[u8]) { self.append_str_by_idx(idx, value); }
    #[inline(always)]
    fn append_int_by_idx(&mut self, idx: usize, value: &[u8]) { self.append_int_by_idx(idx, value); }
    #[inline(always)]
    fn append_float_by_idx(&mut self, idx: usize, value: &[u8]) { self.append_float_by_idx(idx, value); }
    #[inline(always)]
    fn append_null_by_idx(&mut self, idx: usize) { self.append_null_by_idx(idx); }
    #[inline(always)]
    fn append_raw(&mut self, line: &[u8]) { self.append_raw(line); }
    #[inline(always)]
    fn set_buffer(&mut self, buf: &[u8]) { self.set_buffer(buf); }
}

// ---------------------------------------------------------------------------
// Core scan loop — generic over any ScanBuilder
// ---------------------------------------------------------------------------

/// Scan a buffer of newline-delimited JSON into a builder.
///
/// 1. One SIMD pass classifies the entire buffer (~21 GiB/s)
/// 2. Per-line extraction uses pre-computed bitmasks for O(1) string scanning
#[inline(never)]
fn scan_into<B: ScanBuilder>(buf: &[u8], config: &ScanConfig, builder: &mut B) {
    let index = ChunkIndex::new(buf);
    builder.set_buffer(buf);
    builder.begin_batch();

    let mut pos = 0;
    let len = buf.len();

    while pos < len {
        let eol = match memchr(b'\n', &buf[pos..]) {
            Some(offset) => pos + offset,
            None => len,
        };
        let line_start = pos;
        let line_end = eol;
        if line_start < line_end {
            scan_line(buf, line_start, line_end, &index, config, builder);
        }
        pos = eol + 1;
    }
}

/// Scan a single JSON line using the pre-computed chunk index.
#[inline]
fn scan_line<B: ScanBuilder>(
    buf: &[u8],
    start: usize,
    end: usize,
    index: &ChunkIndex,
    config: &ScanConfig,
    builder: &mut B,
) {
    builder.begin_row();

    if config.keep_raw {
        builder.append_raw(&buf[start..end]);
    }

    let mut pos = skip_ws(buf, start, end);
    if pos >= end || buf[pos] != b'{' {
        builder.end_row();
        return;
    }
    pos += 1;

    loop {
        pos = skip_ws(buf, pos, end);
        if pos >= end || buf[pos] == b'}' {
            break;
        }

        // --- Key ---
        if buf[pos] != b'"' {
            break;
        }
        let (key, after_key) = match index.scan_string(buf, pos) {
            Some(r) => r,
            None => break,
        };
        pos = after_key;

        // --- Colon ---
        pos = skip_ws(buf, pos, end);
        if pos >= end || buf[pos] != b':' {
            break;
        }
        pos += 1;
        pos = skip_ws(buf, pos, end);
        if pos >= end {
            break;
        }

        // --- Value ---
        let wanted = config.is_wanted(key);

        let b = buf[pos];
        match b {
            b'"' => {
                let (val, after) = match index.scan_string(buf, pos) {
                    Some(r) => r,
                    None => break,
                };
                if wanted {
                    let idx = builder.resolve_field(key);
                    builder.append_str_by_idx(idx, val);
                }
                pos = after;
            }
            b'{' | b'[' => {
                let val_start = pos;
                pos = index.skip_nested(buf, pos).min(end);
                if wanted {
                    let idx = builder.resolve_field(key);
                    builder.append_str_by_idx(idx, &buf[val_start..pos]);
                }
            }
            b't' | b'f' => {
                let val_start = pos;
                while pos < end
                    && buf[pos] != b','
                    && buf[pos] != b'}'
                    && buf[pos] != b' '
                    && buf[pos] != b'\t'
                {
                    pos += 1;
                }
                if wanted {
                    let idx = builder.resolve_field(key);
                    builder.append_str_by_idx(idx, &buf[val_start..pos]);
                }
            }
            b'n' => {
                pos += 4;
                if pos > end {
                    pos = end;
                }
                if wanted {
                    let idx = builder.resolve_field(key);
                    builder.append_null_by_idx(idx);
                }
            }
            _ => {
                // Number
                let val_start = pos;
                let mut is_float = false;
                while pos < end {
                    let c = buf[pos];
                    if c == b'.' || c == b'e' || c == b'E' {
                        is_float = true;
                    } else if c == b','
                        || c == b'}'
                        || c == b' '
                        || c == b'\t'
                        || c == b'\n'
                        || c == b'\r'
                    {
                        break;
                    }
                    pos += 1;
                }
                if wanted {
                    let val = &buf[val_start..pos];
                    let idx = builder.resolve_field(key);
                    if is_float {
                        builder.append_float_by_idx(idx, val);
                    } else if parse_int_fast(val).is_some() {
                        builder.append_int_by_idx(idx, val);
                    } else {
                        builder.append_float_by_idx(idx, val);
                    }
                }
            }
        }

        // --- Comma ---
        pos = skip_ws(buf, pos, end);
        if pos < end && buf[pos] == b',' {
            pos += 1;
        }
    }

    builder.end_row();
}

#[inline(always)]
fn skip_ws(buf: &[u8], mut pos: usize, end: usize) -> usize {
    while pos < end {
        match buf[pos] {
            b' ' | b'\t' | b'\r' | b'\n' => pos += 1,
            _ => break,
        }
    }
    pos
}

// ---------------------------------------------------------------------------
// SimdScanner — default scanner using IndexedBatchBuilder (3x faster)
// ---------------------------------------------------------------------------

/// Chunk-level SIMD scanner — drop-in replacement for `Scanner`.
///
/// Uses `IndexedBatchBuilder` with bitset-tracked fields and direct
/// index-based appends for ~3x faster builder performance vs HashMap lookup.
pub struct SimdScanner {
    builder: IndexedBatchBuilder,
    config: ScanConfig,
}

impl SimdScanner {
    pub fn new(config: ScanConfig, expected_rows: usize) -> Self {
        SimdScanner {
            builder: IndexedBatchBuilder::new(expected_rows, config.keep_raw),
            config,
        }
    }

    /// Scan a buffer of newline-delimited JSON lines and return a RecordBatch.
    pub fn scan(&mut self, buf: &[u8]) -> RecordBatch {
        scan_into(buf, &self.config, &mut self.builder);
        self.builder.finish_batch()
    }
}

// ---------------------------------------------------------------------------
// ColumnarSimdScanner — zero-copy scanner with adaptive dictionary encoding
// ---------------------------------------------------------------------------

/// SIMD scanner using `ColumnarBatchBuilder` for zero-copy scanning
/// and optional zstd-compressed Arrow IPC output.
///
/// During scanning, stores (offset, len) pointers into the input buffer
/// instead of copying values. On finish, bulk-builds Arrow columns with
/// adaptive dictionary encoding (Dict<Int8>/Dict<Int16>/plain StringArray
/// based on runtime cardinality).
pub struct ColumnarSimdScanner {
    builder: ColumnarBatchBuilder,
    config: ScanConfig,
}

impl ColumnarSimdScanner {
    pub fn new(config: ScanConfig, expected_rows: usize) -> Self {
        ColumnarSimdScanner {
            builder: ColumnarBatchBuilder::new(expected_rows, config.keep_raw),
            config,
        }
    }

    /// Scan and return an uncompressed RecordBatch with adaptive dictionary encoding.
    pub fn scan(&mut self, buf: &[u8]) -> RecordBatch {
        scan_into(buf, &self.config, &mut self.builder);
        self.builder.finish_batch(buf)
    }

    /// Scan and return zstd-compressed Arrow IPC bytes.
    pub fn scan_compressed(&mut self, buf: &[u8]) -> Vec<u8> {
        scan_into(buf, &self.config, &mut self.builder);
        self.builder.finish_batch_compressed(buf)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::FieldSpec;
    use arrow::array::{Array, Float64Array, Int64Array, StringArray};

    fn default_scanner(rows: usize) -> SimdScanner {
        SimdScanner::new(ScanConfig::default(), rows)
    }

    #[test]
    fn test_scan_simple_json() {
        let input = br#"{"host":"web1","status":200,"latency":1.5}
{"host":"web2","status":404,"latency":0.3}
{"host":"web3","status":200,"latency":2.1}
"#;
        let mut s = default_scanner(4);
        let batch = s.scan(input);
        assert_eq!(batch.num_rows(), 3);

        let host = batch
            .column_by_name("host_str")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(host.value(0), "web1");
        assert_eq!(host.value(1), "web2");
        assert_eq!(host.value(2), "web3");

        let status = batch
            .column_by_name("status_int")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(status.value(0), 200);
        assert_eq!(status.value(1), 404);
        assert_eq!(status.value(2), 200);

        let lat = batch
            .column_by_name("latency_float")
            .unwrap()
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert!((lat.value(0) - 1.5).abs() < 1e-10);
    }

    #[test]
    fn test_scan_type_conflict() {
        let input = br#"{"status":200}
{"status":"OK"}
"#;
        let mut s = default_scanner(4);
        let batch = s.scan(input);
        assert_eq!(batch.num_rows(), 2);
        assert!(batch.column_by_name("status_int").is_some());
        assert!(batch.column_by_name("status_str").is_some());
    }

    #[test]
    fn test_scan_missing_fields() {
        let input = br#"{"a":"hello"}
{"b":"world"}
"#;
        let mut s = default_scanner(4);
        let batch = s.scan(input);
        assert_eq!(batch.num_rows(), 2);
        let a = batch
            .column_by_name("a_str")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(a.value(0), "hello");
        assert!(a.is_null(1));
    }

    #[test]
    fn test_scan_nested_json() {
        let input = br#"{"user":{"name":"alice","id":1},"level":"info"}
"#;
        let mut s = default_scanner(4);
        let batch = s.scan(input);
        assert_eq!(batch.num_rows(), 1);
        let user = batch
            .column_by_name("user_str")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let val = user.value(0);
        assert!(val.contains("alice"), "got: {val}");
        assert!(val.starts_with('{'), "got: {val}");
    }

    #[test]
    fn test_scan_config_pushdown() {
        let input =
            br#"{"a":"1","b":"2","c":"3","d":"4","e":"5","f":"6","g":"7","h":"8","i":"9","j":"10"}
"#;
        let config = ScanConfig {
            wanted_fields: vec![
                FieldSpec {
                    name: "a".into(),
                    aliases: vec![],
                },
                FieldSpec {
                    name: "c".into(),
                    aliases: vec![],
                },
            ],
            extract_all: false,
            keep_raw: false,
        };
        let mut s = SimdScanner::new(config, 4);
        let batch = s.scan(input);
        assert_eq!(batch.num_rows(), 1);
        assert!(batch.column_by_name("a_str").is_some());
        assert!(batch.column_by_name("c_str").is_some());
        assert!(batch.column_by_name("b_str").is_none());
        assert_eq!(batch.num_columns(), 2);
    }

    #[test]
    fn test_scan_keep_raw() {
        let line = br#"{"msg":"hello"}"#;
        let input = [line.as_slice(), b"\n"].concat();
        let config = ScanConfig {
            wanted_fields: vec![],
            extract_all: true,
            keep_raw: true,
        };
        let mut s = SimdScanner::new(config, 4);
        let batch = s.scan(&input);
        let raw = batch
            .column_by_name("_raw")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(raw.value(0), r#"{"msg":"hello"}"#);
    }

    #[test]
    fn test_scan_no_raw() {
        let input = br#"{"msg":"hello"}
"#;
        let config = ScanConfig {
            wanted_fields: vec![],
            extract_all: true,
            keep_raw: false,
        };
        let mut s = SimdScanner::new(config, 4);
        let batch = s.scan(input);
        assert!(batch.column_by_name("_raw").is_none());
    }

    #[test]
    fn test_batch_reuse() {
        let input = br#"{"x":1}
{"x":2}
"#;
        let mut s = default_scanner(4);
        let _b1 = s.scan(input);
        let b2 = s.scan(input);
        assert_eq!(b2.num_rows(), 2);
        let col = b2
            .column_by_name("x_int")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(col.value(0), 1);
        assert_eq!(col.value(1), 2);
    }

    #[test]
    fn test_scan_bool_and_null() {
        let input = br#"{"active":true,"deleted":false,"extra":null}
"#;
        let mut s = default_scanner(4);
        let batch = s.scan(input);
        assert_eq!(batch.num_rows(), 1);
        let active = batch
            .column_by_name("active_str")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(active.value(0), "true");
    }

    #[test]
    fn test_scan_negative_int() {
        let input = br#"{"offset":-42}
"#;
        let mut s = default_scanner(4);
        let batch = s.scan(input);
        let col = batch
            .column_by_name("offset_int")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(col.value(0), -42);
    }

    #[test]
    fn test_scan_escaped_string() {
        let input = br#"{"msg":"hello \"world\""}
"#;
        let mut s = default_scanner(4);
        let batch = s.scan(input);
        let col = batch
            .column_by_name("msg_str")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        // Escapes preserved as raw bytes — value contains literal backslash + quote.
        let v = col.value(0);
        assert!(v.contains("world"), "got: {v}");
    }

    #[test]
    fn test_scan_large_batch() {
        let mut input = Vec::with_capacity(100 * 1024);
        for i in 0..1000 {
            let line = format!(
                r#"{{"level":"INFO","msg":"request handled path=/api/v1/users/{}","status":{},"duration_ms":{:.2}}}"#,
                10000 + i,
                if i % 10 == 0 { 500 } else { 200 },
                0.5 + (i as f64) * 0.1
            );
            input.extend_from_slice(line.as_bytes());
            input.push(b'\n');
        }
        let mut s = default_scanner(1024);
        let batch = s.scan(&input);
        assert_eq!(batch.num_rows(), 1000);
        let status = batch
            .column_by_name("status_int")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(status.value(0), 500);
        assert_eq!(status.value(1), 200);
    }

    #[test]
    fn test_duplicate_keys_no_panic() {
        let input = br#"{"a":1,"a":2}
"#;
        let mut s = default_scanner(4);
        let batch = s.scan(input);
        assert_eq!(batch.num_rows(), 1);
        let col = batch
            .column_by_name("a_int")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(col.value(0), 1);
    }

    #[test]
    fn test_i64_overflow_falls_to_float() {
        let input = br#"{"big":99999999999999999999}
"#;
        let mut s = default_scanner(4);
        let batch = s.scan(input);
        assert_eq!(batch.num_rows(), 1);
        assert!(batch.column_by_name("big_float").is_some());
    }

    #[test]
    fn test_array_value() {
        let input = br#"{"tags":["a","b","c"],"n":1}
"#;
        let mut s = default_scanner(4);
        let batch = s.scan(input);
        let tags = batch
            .column_by_name("tags_str")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(tags.value(0), r#"["a","b","c"]"#);
    }

    #[test]
    fn test_empty_object() {
        let input = b"{}\n";
        let mut s = default_scanner(4);
        let batch = s.scan(input);
        assert_eq!(batch.num_rows(), 1);
    }

    #[test]
    fn test_braces_in_nested_string() {
        let input = br#"{"data":{"msg":"has } and { inside"},"ok":true}
"#;
        let mut s = default_scanner(4);
        let batch = s.scan(input);
        assert_eq!(batch.num_rows(), 1);
        let ok = batch
            .column_by_name("ok_str")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(ok.value(0), "true");
    }

    #[test]
    fn test_escape_heavy_multiline() {
        // Multiple lines with varying numbers of escaped quotes
        let mut input = Vec::new();
        for i in 0..10 {
            input.extend_from_slice(br#"{"f":""#);
            for _ in 0..(1 + i) {
                input.extend_from_slice(br#"\""#);
            }
            input.extend_from_slice(br#"","g":"ok"}"#);
            input.push(b'\n');
        }
        let mut s = default_scanner(16);
        let batch = s.scan(&input);
        assert_eq!(batch.num_rows(), 10, "should have 10 rows");
        assert!(batch.column_by_name("f_str").is_some());
        assert!(batch.column_by_name("g_str").is_some());
    }

    // -----------------------------------------------------------------------
    // ColumnarSimdScanner tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_columnar_scan_simple() {
        let input = br#"{"host":"web1","status":200,"latency":1.5}
{"host":"web2","status":404,"latency":0.3}
"#;
        let config = ScanConfig {
            wanted_fields: vec![],
            extract_all: true,
            keep_raw: false,
        };
        let mut s = ColumnarSimdScanner::new(config, 4);
        let batch = s.scan(input);
        assert_eq!(batch.num_rows(), 2);
        // Columnar builder uses dictionary encoding for low-cardinality strings,
        // so we check via the column name and row count.
        assert!(batch.column_by_name("host_str").is_some());
        assert!(batch.column_by_name("status_int").is_some());
        assert!(batch.column_by_name("latency_float").is_some());
    }

    #[test]
    fn test_columnar_scan_compressed() {
        let input = br#"{"level":"INFO","msg":"hello"}
{"level":"WARN","msg":"world"}
"#;
        let config = ScanConfig {
            wanted_fields: vec![],
            extract_all: true,
            keep_raw: false,
        };
        let mut s = ColumnarSimdScanner::new(config, 4);
        let ipc_bytes = s.scan_compressed(input);
        // Verify we got valid IPC data (starts with Arrow magic bytes)
        assert!(!ipc_bytes.is_empty());
        // Decompress and verify
        let cursor = std::io::Cursor::new(ipc_bytes);
        let reader = arrow::ipc::reader::StreamReader::try_new(cursor, None).unwrap();
        let batches: Vec<_> = reader.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 2);
    }

    #[test]
    fn test_columnar_batch_reuse() {
        let input = br#"{"x":"a"}
{"x":"b"}
"#;
        let config = ScanConfig {
            wanted_fields: vec![],
            extract_all: true,
            keep_raw: false,
        };
        let mut s = ColumnarSimdScanner::new(config, 4);
        let _b1 = s.scan(input);
        let b2 = s.scan(input);
        assert_eq!(b2.num_rows(), 2);
    }
}
