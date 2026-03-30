// storage_builder.rs — Self-contained persistence builder.
//
// Collects (row, value) records during scanning, then bulk-builds Arrow
// columns at finish_batch time. No incremental null padding, no cross-builder
// coordination — each column is built independently from its own records.
//
// For the persistence path: scan → build → compress → disk.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, Int64Array, StringDictionaryBuilder, StringBuilder};
use arrow::buffer::NullBuffer;
use arrow::datatypes::{DataType, Field, Int16Type, Int8Type, Schema};
use arrow::error::ArrowError;
use arrow::record_batch::{RecordBatch, RecordBatchOptions};

use crate::scan_config::{parse_float_fast, parse_int_fast};

struct FieldCollector {
    name: Vec<u8>,
    str_values: Vec<(u32, Vec<u8>)>,
    int_values: Vec<(u32, i64)>,
    float_values: Vec<(u32, f64)>,
    has_str: bool,
    has_int: bool,
    has_float: bool,
}

impl FieldCollector {
    fn new(name: &[u8]) -> Self {
        FieldCollector {
            name: name.to_vec(),
            str_values: Vec::with_capacity(256),
            int_values: Vec::with_capacity(256),
            float_values: Vec::with_capacity(256),
            has_str: false,
            has_int: false,
            has_float: false,
        }
    }
    fn clear(&mut self) {
        self.str_values.clear();
        self.int_values.clear();
        self.float_values.clear();
        self.has_str = false;
        self.has_int = false;
        self.has_float = false;
    }
}

/// Self-contained persistence builder.
///
/// Collects `(row, value)` records during scanning, then bulk-builds Arrow
/// columns at `finish_batch` time. Each column is independent — no cross-builder
/// null padding, correct by construction.
///
/// Use for: scan → build → compress → disk queue.
pub struct StorageBuilder {
    fields: Vec<FieldCollector>,
    field_index: HashMap<Vec<u8>, usize>,
    raw_values: Vec<Vec<u8>>,
    row_count: u32,
    keep_raw: bool,
    written_bits: u64,
}

impl StorageBuilder {
    pub fn new(keep_raw: bool) -> Self {
        StorageBuilder {
            fields: Vec::with_capacity(32),
            field_index: HashMap::with_capacity(32),
            raw_values: Vec::new(),
            row_count: 0,
            keep_raw,
            written_bits: 0,
        }
    }

    pub fn begin_batch(&mut self) {
        self.row_count = 0;
        for fc in &mut self.fields {
            fc.clear();
        }
        self.raw_values.clear();
    }

    #[inline(always)]
    pub fn begin_row(&mut self) {
        self.written_bits = 0;
    }

    #[inline(always)]
    pub fn end_row(&mut self) {
        self.row_count += 1;
    }

    #[inline]
    pub fn resolve_field(&mut self, key: &[u8]) -> usize {
        if let Some(&idx) = self.field_index.get(key) {
            return idx;
        }
        let idx = self.fields.len();
        self.fields.push(FieldCollector::new(key));
        self.field_index.insert(key.to_vec(), idx);
        idx
    }

    #[inline(always)]
    fn check_dup(&mut self, idx: usize) -> bool {
        if idx < 64 {
            let bit = 1u64 << idx;
            if self.written_bits & bit != 0 {
                return true;
            }
            self.written_bits |= bit;
            false
        } else {
            false
        }
    }

    #[inline(always)]
    pub fn append_str_by_idx(&mut self, idx: usize, value: &[u8]) {
        if self.check_dup(idx) {
            return;
        }
        // StringArray requires valid UTF-8.  JSON is always UTF-8 in production;
        // for fuzz / corrupted input we skip non-UTF-8 bytes rather than storing
        // them and triggering UB later in finish_batch.
        if std::str::from_utf8(value).is_err() {
            return;
        }
        let fc = &mut self.fields[idx];
        fc.has_str = true;
        fc.str_values.push((self.row_count, value.to_vec()));
    }

    #[inline(always)]
    pub fn append_int_by_idx(&mut self, idx: usize, value: &[u8]) {
        if self.check_dup(idx) {
            return;
        }
        let fc = &mut self.fields[idx];
        if let Some(v) = parse_int_fast(value) {
            fc.has_int = true;
            fc.int_values.push((self.row_count, v));
        }
    }

    #[inline(always)]
    pub fn append_float_by_idx(&mut self, idx: usize, value: &[u8]) {
        if self.check_dup(idx) {
            return;
        }
        let fc = &mut self.fields[idx];
        if let Some(v) = parse_float_fast(value) {
            fc.has_float = true;
            fc.float_values.push((self.row_count, v));
        }
    }

    #[inline(always)]
    pub fn append_null_by_idx(&mut self, idx: usize) {
        // Mark as written so duplicate keys are detected.
        // Null values are implicit (absence of a record = null at finish_batch).
        self.check_dup(idx);
    }

    #[inline]
    pub fn append_raw(&mut self, line: &[u8]) {
        if self.keep_raw {
            self.raw_values.push(line.to_vec());
        }
    }

    pub fn finish_batch(&mut self) -> Result<RecordBatch, ArrowError> {
        let num_rows = self.row_count as usize;
        let mut schema_fields: Vec<Field> = Vec::with_capacity(self.fields.len() + 1);
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(self.fields.len() + 1);

        for fc in &self.fields {
            // Field names come from JSON keys (valid UTF-8 in well-formed input).
            // Use from_utf8_lossy so that fuzz inputs with arbitrary bytes are
            // handled gracefully instead of triggering undefined behaviour.
            let name = String::from_utf8_lossy(&fc.name);

            if fc.has_int {
                let mut values = vec![0i64; num_rows];
                let mut valid = vec![false; num_rows];
                for &(row, v) in &fc.int_values {
                    let r = row as usize;
                    if r < num_rows {
                        values[r] = v;
                        valid[r] = true;
                    }
                }
                schema_fields.push(Field::new(format!("{}_int", name), DataType::Int64, true));
                arrays.push(Arc::new(Int64Array::new(
                    values.into(),
                    Some(NullBuffer::from(valid)),
                )) as ArrayRef);
            }
            if fc.has_float {
                let mut values = vec![0.0f64; num_rows];
                let mut valid = vec![false; num_rows];
                for &(row, v) in &fc.float_values {
                    let r = row as usize;
                    if r < num_rows {
                        values[r] = v;
                        valid[r] = true;
                    }
                }
                schema_fields.push(Field::new(
                    format!("{}_float", name),
                    DataType::Float64,
                    true,
                ));
                arrays.push(Arc::new(Float64Array::new(
                    values.into(),
                    Some(NullBuffer::from(valid)),
                )) as ArrayRef);
            }
            if fc.has_str {
                // Count unique values to select the most compact encoding.
                let unique_count: usize = fc
                    .str_values
                    .iter()
                    .map(|(_, v)| v.as_slice())
                    .collect::<HashSet<&[u8]>>()
                    .len();

                let col_name = format!("{}_str", name);

                if unique_count < 128 {
                    // Very low cardinality — Int8 dictionary keys (1 byte/row).
                    let mut builder = StringDictionaryBuilder::<Int8Type>::new();
                    let mut vi = 0;
                    for row in 0..num_rows {
                        if vi < fc.str_values.len() && fc.str_values[vi].0 as usize == row {
                            let s = String::from_utf8_lossy(&fc.str_values[vi].1);
                            builder.append_value(&*s);
                            vi += 1;
                        } else {
                            builder.append_null();
                        }
                    }
                    schema_fields.push(Field::new(
                        col_name,
                        DataType::Dictionary(
                            Box::new(DataType::Int8),
                            Box::new(DataType::Utf8),
                        ),
                        true,
                    ));
                    arrays.push(Arc::new(builder.finish()) as ArrayRef);
                } else if unique_count < 32768 && unique_count * 2 < num_rows {
                    // Moderate cardinality and dictionary is smaller than 50% of
                    // rows — Int16 dictionary keys (2 bytes/row).
                    let mut builder = StringDictionaryBuilder::<Int16Type>::new();
                    let mut vi = 0;
                    for row in 0..num_rows {
                        if vi < fc.str_values.len() && fc.str_values[vi].0 as usize == row {
                            let s = String::from_utf8_lossy(&fc.str_values[vi].1);
                            builder.append_value(&*s);
                            vi += 1;
                        } else {
                            builder.append_null();
                        }
                    }
                    schema_fields.push(Field::new(
                        col_name,
                        DataType::Dictionary(
                            Box::new(DataType::Int16),
                            Box::new(DataType::Utf8),
                        ),
                        true,
                    ));
                    arrays.push(Arc::new(builder.finish()) as ArrayRef);
                } else {
                    // High cardinality or near-unique — plain UTF-8 string.
                    let mut builder = StringBuilder::with_capacity(num_rows, num_rows * 16);
                    let mut vi = 0;
                    for row in 0..num_rows {
                        if vi < fc.str_values.len() && fc.str_values[vi].0 as usize == row {
                            // String values come from the JSON input.  Use
                            // from_utf8_lossy so that non-UTF-8 fuzz input is
                            // handled safely (replacement characters) rather than
                            // invoking undefined behaviour.
                            let s = String::from_utf8_lossy(&fc.str_values[vi].1);
                            builder.append_value(&*s);
                            vi += 1;
                        } else {
                            builder.append_null();
                        }
                    }
                    schema_fields.push(Field::new(col_name, DataType::Utf8, true));
                    arrays.push(Arc::new(builder.finish()) as ArrayRef);
                }
            }
        }

        if self.keep_raw && !self.raw_values.is_empty() {
            let mut builder = StringBuilder::with_capacity(num_rows, num_rows * 128);
            for val in &self.raw_values {
                builder.append_value(&*String::from_utf8_lossy(val));
            }
            for _ in self.raw_values.len()..num_rows {
                builder.append_null();
            }
            schema_fields.push(Field::new("_raw", DataType::Utf8, true));
            arrays.push(Arc::new(builder.finish()) as ArrayRef);
        }

        let schema = Arc::new(Schema::new(schema_fields));
        let opts = RecordBatchOptions::new().with_row_count(Some(num_rows));
        RecordBatch::try_new_with_options(schema, arrays, &opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Array, DictionaryArray, StringArray};
    use arrow::datatypes::Int8Type;

    /// Returns the string value at `row` for a column that may be either a
    /// `DictionaryArray<Int8Type>` (low-cardinality path) or a plain
    /// `StringArray` (high-cardinality path).
    fn str_value<'a>(col: &'a dyn Array, row: usize) -> &'a str {
        if let Some(dict) = col.as_any().downcast_ref::<DictionaryArray<Int8Type>>() {
            let key = dict.keys().value(row) as usize;
            return dict
                .values()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(key);
        }
        col.as_any().downcast_ref::<StringArray>().unwrap().value(row)
    }

    #[test]
    fn test_basic() {
        let mut b = StorageBuilder::new(false);
        b.begin_batch();
        let h = b.resolve_field(b"host");
        let s = b.resolve_field(b"status");
        b.begin_row();
        b.append_str_by_idx(h, b"web1");
        b.append_int_by_idx(s, b"200");
        b.end_row();
        b.begin_row();
        b.append_str_by_idx(h, b"web2");
        b.append_int_by_idx(s, b"404");
        b.end_row();
        let batch = b.finish_batch().unwrap();
        assert_eq!(batch.num_rows(), 2);
        let host_col = batch.column_by_name("host_str").unwrap();
        assert_eq!(str_value(host_col.as_ref(), 0), "web1");
        assert_eq!(
            batch
                .column_by_name("status_int")
                .unwrap()
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(1),
            404
        );
    }

    #[test]
    fn test_missing_fields() {
        let mut b = StorageBuilder::new(false);
        b.begin_batch();
        let a = b.resolve_field(b"a");
        b.begin_row();
        b.append_str_by_idx(a, b"hello");
        b.end_row();
        let bx = b.resolve_field(b"b");
        b.begin_row();
        b.append_str_by_idx(bx, b"world");
        b.end_row();
        let batch = b.finish_batch().unwrap();
        assert_eq!(batch.num_rows(), 2);
        let ac = batch.column_by_name("a_str").unwrap();
        assert!(!ac.is_null(0));
        assert!(ac.is_null(1));
    }

    #[test]
    fn test_type_conflict() {
        let mut b = StorageBuilder::new(false);
        b.begin_batch();
        let idx = b.resolve_field(b"status");
        b.begin_row();
        b.append_int_by_idx(idx, b"200");
        b.end_row();
        b.begin_row();
        b.append_str_by_idx(idx, b"OK");
        b.end_row();
        let batch = b.finish_batch().unwrap();
        assert!(batch.column_by_name("status_int").is_some());
        assert!(batch.column_by_name("status_str").is_some());
    }

    #[test]
    fn test_many_fields_across_lines() {
        let mut b = StorageBuilder::new(false);
        b.begin_batch();
        let a = b.resolve_field(b"a");
        let bx = b.resolve_field(b"b");
        let c = b.resolve_field(b"c");
        b.begin_row();
        b.append_str_by_idx(a, b"1");
        b.append_str_by_idx(bx, b"2");
        b.append_str_by_idx(c, b"3");
        b.end_row();
        let d = b.resolve_field(b"d");
        let e = b.resolve_field(b"e");
        b.begin_row();
        b.append_str_by_idx(d, b"4");
        b.append_int_by_idx(e, b"5");
        b.end_row();
        let f = b.resolve_field(b"f");
        b.begin_row();
        b.append_str_by_idx(a, b"6");
        b.append_float_by_idx(f, b"7.0");
        b.end_row();
        let batch = b.finish_batch().unwrap();
        assert_eq!(batch.num_rows(), 3);
        let ac = batch.column_by_name("a_str").unwrap();
        assert!(!ac.is_null(0));
        assert!(ac.is_null(1));
        assert!(!ac.is_null(2));
    }

    #[test]
    fn test_duplicate_keys() {
        let mut b = StorageBuilder::new(false);
        b.begin_batch();
        let idx = b.resolve_field(b"a");
        b.begin_row();
        b.append_int_by_idx(idx, b"1");
        b.append_int_by_idx(idx, b"2");
        b.end_row();
        let batch = b.finish_batch().unwrap();
        assert_eq!(
            batch
                .column_by_name("a_int")
                .unwrap()
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(0),
            1
        );
    }

    #[test]
    fn test_empty_batch() {
        let mut b = StorageBuilder::new(false);
        b.begin_batch();
        let batch = b.finish_batch().unwrap();
        assert_eq!(batch.num_rows(), 0);
    }

    /// Verify that a single repeated value (cardinality = 1) is stored as an
    /// `Int8` dictionary — the primary motivating case from the issue
    /// (`level` column with one unique value).
    #[test]
    fn test_adaptive_encoding_int8_dict() {
        let mut b = StorageBuilder::new(false);
        b.begin_batch();
        let lvl = b.resolve_field(b"level");
        for _ in 0..10 {
            b.begin_row();
            b.append_str_by_idx(lvl, b"info");
            b.end_row();
        }
        let batch = b.finish_batch().unwrap();
        let col = batch.column_by_name("level_str").unwrap();
        // Must be an Int8 dictionary, not a plain StringArray.
        assert_eq!(
            col.data_type(),
            &DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Utf8))
        );
        // All rows must read back as "info" via the dictionary.
        let dict = col
            .as_any()
            .downcast_ref::<DictionaryArray<Int8Type>>()
            .unwrap();
        let values = dict
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for i in 0..10 {
            let key = dict.keys().value(i) as usize;
            assert_eq!(values.value(key), "info");
        }
    }

    /// Verify that a high-cardinality field (every row has a unique value) is
    /// stored as plain UTF-8 rather than a dictionary.
    #[test]
    fn test_adaptive_encoding_plain_string() {
        let mut b = StorageBuilder::new(false);
        b.begin_batch();
        let msg = b.resolve_field(b"msg");
        // 200 unique values — each row different, cardinality == num_rows.
        for i in 0u32..200 {
            b.begin_row();
            b.append_str_by_idx(msg, format!("msg-{i}").as_bytes());
            b.end_row();
        }
        let batch = b.finish_batch().unwrap();
        let col = batch.column_by_name("msg_str").unwrap();
        // Must be a plain StringArray.
        assert_eq!(col.data_type(), &DataType::Utf8);
        let sa = col.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(sa.value(0), "msg-0");
        assert_eq!(sa.value(199), "msg-199");
    }

    /// Verify that a moderate-cardinality field where unique < 32 768 and
    /// unique < 50 % of rows is stored as an Int16 dictionary.
    #[test]
    fn test_adaptive_encoding_int16_dict() {
        use arrow::array::DictionaryArray;
        use arrow::datatypes::Int16Type;

        let mut b = StorageBuilder::new(false);
        b.begin_batch();
        let svc = b.resolve_field(b"svc");
        // 130 unique values across 1000 rows: 130 < 32 768 and 130*2 < 1000.
        let num_rows = 1000usize;
        let num_unique = 130usize;
        for i in 0..num_rows {
            b.begin_row();
            let s = format!("svc-{}", i % num_unique);
            b.append_str_by_idx(svc, s.as_bytes());
            b.end_row();
        }
        let batch = b.finish_batch().unwrap();
        let col = batch.column_by_name("svc_str").unwrap();
        assert_eq!(
            col.data_type(),
            &DataType::Dictionary(Box::new(DataType::Int16), Box::new(DataType::Utf8))
        );
        let dict = col
            .as_any()
            .downcast_ref::<DictionaryArray<Int16Type>>()
            .unwrap();
        // Dictionary should have exactly 130 unique entries.
        assert_eq!(dict.values().len(), num_unique);
    }
}
