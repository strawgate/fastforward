// materialize.rs — Detach a RecordBatch from its input buffer.
//
// StreamingBuilder produces StringViewArray columns that reference the
// original input `Bytes` buffer. Before the input can be freed (or the
// batch compressed for persistence), string data must be copied into
// independent buffers.
//
// DataFusion's WHERE filter already does this implicitly (the filter
// kernel copies selected rows into fresh arrays). For passthrough
// queries (SELECT * without WHERE), an explicit materialization step
// is needed.
//
// `materialize_if_pinned` checks whether any column still references
// the input buffer and only copies when necessary — zero overhead
// when a filter already detached the data.

use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;

/// Unconditionally materialize all `Utf8View` columns to `Utf8`.
///
/// Produces a self-contained `RecordBatch` whose string columns are
/// contiguous `StringArray` instead of `StringViewArray`. The input
/// buffer can be freed after this call.
///
/// If no `Utf8View` columns exist, returns a cheap clone (Arc bumps only).
pub fn materialize(batch: &RecordBatch) -> RecordBatch {
    let schema = batch.schema();
    let has_views = schema
        .fields()
        .iter()
        .any(|f| *f.data_type() == DataType::Utf8View);
    if !has_views {
        return batch.clone();
    }
    cast_views(batch)
}

/// Materialize only if at least one column still references `input_buf`.
///
/// After a DataFusion WHERE filter, string columns are typically already
/// independent of the input buffer. In that case this function returns
/// the batch unchanged (zero copy). Only when columns still hold views
/// into the input does it perform the bulk cast.
///
/// This is the optimal persistence boundary: zero cost when the SQL
/// transform already detached the data, bulk copy otherwise.
pub fn materialize_if_pinned(batch: &RecordBatch, input_buf: &bytes::Bytes) -> RecordBatch {
    if is_pinned(batch, input_buf) {
        cast_views(batch)
    } else {
        batch.clone()
    }
}

/// Check whether any column buffer in the batch overlaps the input buffer.
pub fn is_pinned(batch: &RecordBatch, input_buf: &bytes::Bytes) -> bool {
    let buf_start = input_buf.as_ptr() as usize;
    let buf_end = buf_start + input_buf.len();

    for col in batch.columns() {
        let arr_data = col.to_data();
        for buffer in arr_data.buffers() {
            let ptr = buffer.as_ptr() as usize;
            let end = ptr + buffer.len();
            // Check overlap: buffer range intersects input range
            if ptr < buf_end && end > buf_start {
                return true;
            }
        }
    }
    false
}

/// Cast all Utf8View columns to Utf8 (StringArray).
fn cast_views(batch: &RecordBatch) -> RecordBatch {
    let schema = batch.schema();
    let mut columns = Vec::with_capacity(batch.num_columns());
    let mut fields = Vec::with_capacity(batch.num_columns());
    for (i, field) in schema.fields().iter().enumerate() {
        if *field.data_type() == DataType::Utf8View {
            let utf8_col =
                arrow::compute::cast(batch.column(i), &DataType::Utf8).expect("Utf8View→Utf8 cast");
            fields.push(Arc::new(Field::new(
                field.name(),
                DataType::Utf8,
                field.is_nullable(),
            )));
            columns.push(utf8_col);
        } else {
            fields.push(Arc::clone(field));
            columns.push(Arc::clone(batch.column(i)));
        }
    }
    let new_schema = Arc::new(Schema::new(fields));
    RecordBatch::try_new(new_schema, columns).expect("schema/column mismatch after cast")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StreamingSimdScanner;
    use bytes::Bytes;
    use logfwd_core::scan_config::ScanConfig;

    fn scan(input: &[u8]) -> (RecordBatch, Bytes) {
        let buf = Bytes::from(input.to_vec());
        let mut scanner = StreamingSimdScanner::new(ScanConfig::default());
        let batch = scanner.scan(buf.clone()).unwrap();
        (batch, buf)
    }

    #[test]
    fn is_pinned_true_after_scan() {
        let (batch, buf) = scan(b"{\"msg\":\"hello\"}\n");
        assert!(is_pinned(&batch, &buf));
    }

    #[test]
    fn is_pinned_false_after_materialize() {
        let (batch, buf) = scan(b"{\"msg\":\"hello\"}\n");
        let owned = materialize(&batch);
        assert!(!is_pinned(&owned, &buf));
    }

    #[test]
    fn materialize_if_pinned_copies_when_pinned() {
        let (batch, buf) = scan(b"{\"msg\":\"hello\"}\n");
        let result = materialize_if_pinned(&batch, &buf);
        assert!(!is_pinned(&result, &buf));
        assert_eq!(result.num_rows(), 1);
    }

    #[test]
    fn materialize_if_pinned_skips_when_not_pinned() {
        let (batch, buf) = scan(b"{\"msg\":\"hello\"}\n");
        let owned = materialize(&batch);
        // owned is already detached — materialize_if_pinned should be a no-op
        let result = materialize_if_pinned(&owned, &buf);
        assert_eq!(result.num_rows(), 1);
    }

    #[test]
    fn int_only_batch_not_pinned() {
        let (batch, buf) = scan(b"{\"n\":42}\n");
        // Int64 columns don't reference the input buffer
        // (values are parsed during scan, not stored as views)
        // But the scanner may still have metadata referencing the buffer.
        // After materialize, definitely not pinned.
        let owned = materialize(&batch);
        assert!(!is_pinned(&owned, &buf));
    }

    #[test]
    fn preserves_data_after_materialize() {
        let (batch, _buf) = scan(b"{\"msg\":\"hello\",\"n\":42}\n");
        let owned = materialize(&batch);
        assert_eq!(owned.num_rows(), 1);
        assert_eq!(owned.num_columns(), batch.num_columns());

        // Verify string data survived
        let msg_col = owned.column_by_name("msg").expect("msg column");
        let arr = msg_col
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .expect("should be StringArray after materialize");
        assert_eq!(arr.value(0), "hello");

        // Verify int data survived
        let n_col = owned.column_by_name("n").expect("n column");
        let int_arr = n_col
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .expect("should be Int64Array");
        assert_eq!(int_arr.value(0), 42);
    }
}
