//! Prometheus text exposition → Arrow `RecordBatch`.
//!
//! [`PromBatchBuilder`](crate::prom::PromBatchBuilder) drives [`ffwd_core::prom_text::scan_prom_text`] into a
//! purpose-built dense column builder (`PromColumns`). The scanner validates
//! the whole scrape body as UTF-8 once up front, so every metric name, label
//! value and `# TYPE` token becomes a `Utf8View` without re-validation:
//! strings of at most 12 bytes are stored inline in the view, longer ones
//! reference the scrape buffer zero-copy, and strings with escapes are
//! decoded into a second buffer that their views reference instead.
//!
//! Output schema (one row per sample):
//!
//! | column             | type      | notes                                   |
//! |--------------------|-----------|-----------------------------------------|
//! | `__name__`         | Utf8View  | metric name                             |
//! | `<label>`          | Utf8View  | one nullable column per label name      |
//! | `__value__`        | Float64   | sample value                            |
//! | `__timestamp_ms__` | Int64     | only when the exposition carries one    |
//! | `__type__`         | Utf8View  | `# TYPE` of the family, when declared   |

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, Int64Array, StringViewArray};
use arrow::buffer::{BooleanBuffer, Buffer, NullBuffer, ScalarBuffer};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::error::ArrowError;
use arrow::record_batch::{RecordBatch, RecordBatchOptions};
use bytes::Bytes;
use ffwd_core::prom_text::{
    PROM_NAME_FIELD, PROM_TIMESTAMP_FIELD, PROM_TYPE_FIELD, PROM_VALUE_FIELD, PromBuilder,
    PromErrorKind, PromParseError, PromScanConfig, PromScanStats, PromScratch, scan_prom_text,
};
use ffwd_core::scanner::ScanBuilder;

/// Column holding a stable 64-bit identity of each sample's series.
///
/// The id hashes the metric name and every non-empty label (column name and
/// value), so it does not depend on label order or on which other columns
/// the batch happens to contain. Use it to group, join or deduplicate series
/// without comparing every label column.
pub const PROM_SERIES_ID_FIELD: &str = "__series_id__";

/// Columns a Prometheus batch always types as non-strings. SQL validation
/// probes, which have no real batch to read types from, use this list so
/// arithmetic such as `__value__ + 1` plans the same way it runs.
pub static PROM_TYPED_COLUMNS: [(&str, DataType); 3] = [
    ("__value__", DataType::Float64),
    ("__timestamp_ms__", DataType::Int64),
    (PROM_SERIES_ID_FIELD, DataType::UInt64),
];

/// Default limit on distinct label columns in one body.
pub const DEFAULT_MAX_LABEL_COLUMNS: usize = 1000;
/// Default limit on null cells padded into sparse columns for one body
/// (16 Mi cells, 256 MiB of string views at worst).
pub const DEFAULT_MAX_PADDED_CELLS: usize = 1 << 24;

/// Seed for the series hash. Changing it changes every series id.
const SERIES_SEED: u64 = 0x6666_7764_7072_6f6d;

/// Return `batch` with a [`PROM_SERIES_ID_FIELD`] `UInt64` column appended.
///
/// Label columns are the string columns other than `__name__` and `__type__`
/// (and any column whose name starts with `__`). Each non-empty label
/// contributes `xxh64(value, seed = xxh64(column name))`; contributions are
/// summed, which makes the id independent of column order. Empty values are
/// skipped because Prometheus treats `a=""` as no label `a`. Rows without a
/// name hash only their labels. Schema metadata is kept.
pub fn append_series_id(batch: &RecordBatch) -> Result<RecordBatch, ArrowError> {
    use arrow::array::{Array, AsArray, UInt64Array};
    use xxhash_rust::xxh64::xxh64;

    let num_rows = batch.num_rows();
    let mut ids = vec![SERIES_SEED; num_rows];
    let schema = batch.schema();
    for (field, col) in schema.fields().iter().zip(batch.columns()) {
        let name = field.name().as_str();
        let is_name = name == "__name__";
        if !is_name && name.starts_with("__") {
            continue;
        }
        let col_seed = xxh64(name.as_bytes(), SERIES_SEED);
        let mut add = |row: usize, value: &[u8]| {
            if value.is_empty() {
                return;
            }
            let h = xxh64(value, col_seed);
            if let Some(id) = ids.get_mut(row) {
                *id = id.wrapping_add(h.rotate_left(17) ^ h);
            }
        };
        match col.data_type() {
            DataType::Utf8View => {
                let a = col.as_string_view();
                for row in 0..num_rows {
                    if a.is_valid(row) {
                        add(row, a.value(row).as_bytes());
                    }
                }
            }
            DataType::Utf8 => {
                let a = col.as_string::<i32>();
                for row in 0..num_rows {
                    if a.is_valid(row) {
                        add(row, a.value(row).as_bytes());
                    }
                }
            }
            DataType::LargeUtf8 => {
                let a = col.as_string::<i64>();
                for row in 0..num_rows {
                    if a.is_valid(row) {
                        add(row, a.value(row).as_bytes());
                    }
                }
            }
            _ => {}
        }
    }
    let mut fields: Vec<arrow::datatypes::FieldRef> = schema.fields().iter().cloned().collect();
    let mut arrays: Vec<ArrayRef> = batch.columns().to_vec();
    if let Some(pos) = fields.iter().position(|f| f.name() == PROM_SERIES_ID_FIELD) {
        fields.remove(pos);
        arrays.remove(pos);
    }
    fields.push(Arc::new(Field::new(
        PROM_SERIES_ID_FIELD,
        DataType::UInt64,
        false,
    )));
    arrays.push(Arc::new(UInt64Array::from(ids)));
    let opts = RecordBatchOptions::new().with_row_count(Some(num_rows));
    let schema = Schema::new_with_metadata(fields, schema.metadata().clone());
    RecordBatch::try_new_with_options(Arc::new(schema), arrays, &opts)
}

/// Errors from [`PromBatchBuilder::scan`].
#[derive(Debug)]
pub enum PromToArrowError {
    /// The scrape body is not valid UTF-8 (required by the text format).
    InvalidUtf8,
    /// The body is larger than the 4 GiB addressable by string views.
    TooLarge,
    /// The body has more label columns than the configured limit.
    TooManyLabelColumns {
        /// The configured limit.
        limit: usize,
    },
    /// A sample repeats a label name (`m{a="1",a="2"}`).
    DuplicateLabel,
    /// Sparse columns (labels, plus the synthetic `__timestamp_ms__` and
    /// `__type__` columns) would need more null padding than the limit,
    /// i.e. the dense output would be far larger than the body.
    TooManyPaddedCells {
        /// The configured limit.
        limit: usize,
    },
    /// The exposition is malformed; the whole scrape is rejected.
    Parse(PromParseError),
    /// Arrow materialization failed.
    Arrow(ArrowError),
}

impl std::fmt::Display for PromToArrowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUtf8 => f.write_str("exposition is not valid UTF-8"),
            Self::TooLarge => f.write_str("exposition exceeds 4 GiB"),
            Self::DuplicateLabel => f.write_str("a sample repeats a label name"),
            Self::TooManyLabelColumns { limit } => {
                write!(f, "label columns exceed max_label_columns ({limit})")
            }
            Self::TooManyPaddedCells { limit } => write!(
                f,
                "sparse columns (labels, timestamps or types present on only \
                 some samples) would pad more than {limit} null cells; \
                 reduce label cardinality or split the target"
            ),
            Self::Parse(e) => write!(
                f,
                "invalid exposition at line {} (byte {}): {:?}",
                e.line, e.offset, e.kind
            ),
            Self::Arrow(e) => write!(f, "arrow materialization failed: {e}"),
        }
    }
}

impl std::error::Error for PromToArrowError {}

/// One output column. Dense storage indexed by row; rows that never wrote
/// the column are zero (an empty inline string view / 0) with a cleared
/// validity bit.
struct Column<T> {
    name: String,
    values: Vec<T>,
    /// Validity bitmap, one bit per row, grown lazily.
    valid: Vec<u64>,
    /// Number of rows with a value; equals `num_rows` for dense columns.
    count: usize,
}

impl<T: Copy + Default> Column<T> {
    fn new(name: String) -> Self {
        Self {
            name,
            values: Vec::new(),
            valid: Vec::new(),
            count: 0,
        }
    }

    /// First write wins: a second write to the same row is ignored. Null
    /// padding for skipped rows is taken from `budget`; a write whose padding
    /// does not fit is dropped and the budget marked exhausted.
    #[inline(always)]
    #[allow(clippy::indexing_slicing)]
    fn set(&mut self, row: usize, value: T, budget: &mut PadBudget) {
        if self.values.len() > row {
            return;
        }
        if self.values.len() < row {
            if !budget.take(row - self.values.len()) {
                return;
            }
            self.values.resize(row, T::default());
        }
        self.values.push(value);
        let word = row >> 6;
        if self.valid.len() <= word {
            self.valid.resize(word + 1, 0);
        }
        self.valid[word] |= 1u64 << (row & 63);
        self.count += 1;
    }

    /// Pad to `num_rows` and hand out (values, validity).
    fn take(&mut self, num_rows: usize) -> (Vec<T>, Option<NullBuffer>) {
        let mut values = std::mem::take(&mut self.values);
        values.resize(num_rows, T::default());
        let nulls = if self.count == num_rows {
            None
        } else {
            let mut words = std::mem::take(&mut self.valid);
            words.resize(num_rows.div_ceil(64), 0);
            let bits = BooleanBuffer::new(Buffer::from_vec(words), 0, num_rows);
            Some(NullBuffer::new(bits))
        };
        self.valid.clear();
        self.count = 0;
        (values, nulls)
    }
}

/// Null cells that sparse columns may pad, per body.
struct PadBudget {
    limit: usize,
    left: usize,
    exhausted: bool,
}

impl PadBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            left: limit,
            exhausted: false,
        }
    }

    fn reset(&mut self) {
        self.left = self.limit;
        self.exhausted = false;
    }

    #[cold]
    fn take(&mut self, cells: usize) -> bool {
        match self.left.checked_sub(cells) {
            Some(rest) => {
                self.left = rest;
                true
            }
            None => {
                self.exhausted = true;
                false
            }
        }
    }
}

/// Build an Arrow string view: strings of at most 12 bytes are inlined,
/// longer ones reference `block` at `offset` with a 4-byte prefix.
#[inline(always)]
#[allow(clippy::indexing_slicing)]
fn make_view(bytes: &[u8], block: u32, offset: u32) -> u128 {
    let len = bytes.len();
    if len <= 12 {
        let mut v = [0u8; 16];
        v[..4].copy_from_slice(&(len as u32).to_le_bytes());
        v[4..4 + len].copy_from_slice(bytes);
        u128::from_le_bytes(v)
    } else {
        let prefix = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        (len as u128)
            | (u128::from(prefix) << 32)
            | (u128::from(block) << 64)
            | (u128::from(offset) << 96)
    }
}

/// Purpose-built Arrow builder for Prometheus samples.
///
/// Prometheus rows are narrow and regular: `__name__` and `__value__` are
/// written on every row and label columns repeat per family. Storing values
/// densely by row (instead of `(row, value)` facts) makes each write a
/// `Vec::push` and finishing a batch a move of the vectors into Arrow.
struct PromColumns {
    buf: Bytes,
    decoded: Vec<u8>,
    strs: Vec<Column<u128>>,
    f64s: Vec<Column<f64>>,
    i64s: Vec<Column<i64>>,
    /// Scanner column index → (kind, position in the typed vector).
    slots: Vec<(Kind, usize)>,
    index: HashMap<Vec<u8>, usize>,
    row: usize,
    /// Maximum number of label columns (every column but the synthetic
    /// `__name__`, `__value__`, `__timestamp_ms__` and `__type__`).
    max_labels: usize,
    /// Null padding allowed for sparse columns, scan and finish combined.
    pad: PadBudget,
    labels: usize,
    /// Slot that swallows writes once `max_labels` is exceeded, so a
    /// label-heavy body stops allocating new columns.
    drop_slot: Option<usize>,
    /// Set when a sample wrote the same label column twice.
    has_duplicate: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    F64,
    I64,
    Drop,
}

impl PromColumns {
    fn new() -> Self {
        Self {
            buf: Bytes::new(),
            decoded: Vec::new(),
            strs: Vec::new(),
            f64s: Vec::new(),
            i64s: Vec::new(),
            slots: Vec::new(),
            index: HashMap::new(),
            row: 0,
            max_labels: DEFAULT_MAX_LABEL_COLUMNS,
            pad: PadBudget::new(DEFAULT_MAX_PADDED_CELLS),
            labels: 0,
            drop_slot: None,
            has_duplicate: false,
        }
    }

    fn is_over_limit(&self) -> bool {
        self.drop_slot.is_some()
    }

    fn begin_batch(&mut self, buf: Bytes) {
        self.buf = buf;
        self.decoded.clear();
        self.strs.clear();
        self.f64s.clear();
        self.i64s.clear();
        self.slots.clear();
        self.index.clear();
        self.row = 0;
        self.labels = 0;
        self.drop_slot = None;
        self.has_duplicate = false;
        self.pad.reset();
    }

    /// Charge the padding `finish` will add to the budget. False when the
    /// budget is (or would be) exhausted.
    fn reserve_finish_padding(&mut self) -> bool {
        let rows = self.row;
        // Columns that were never written are left out of the batch.
        let pad = |len: usize, count: usize| {
            if count == 0 {
                0
            } else {
                rows.saturating_sub(len)
            }
        };
        let missing = self
            .strs
            .iter()
            .map(|c| pad(c.values.len(), c.count))
            .chain(self.f64s.iter().map(|c| pad(c.values.len(), c.count)))
            .chain(self.i64s.iter().map(|c| pad(c.values.len(), c.count)))
            .fold(0usize, usize::saturating_add);
        !self.pad.exhausted && self.pad.take(missing)
    }

    #[inline(always)]
    #[allow(clippy::indexing_slicing)]
    fn slot(&self, idx: usize) -> (Kind, usize) {
        self.slots[idx]
    }

    fn finish(&mut self) -> Result<RecordBatch, ArrowError> {
        let num_rows = self.row;
        // Hand the body to the arrays: the builder keeps no reference, so a
        // large response is freed as soon as the batch is dropped.
        let original = Buffer::from(std::mem::take(&mut self.buf));
        let decoded = Buffer::from_vec(std::mem::take(&mut self.decoded));
        let blocks: Arc<[Buffer]> = Arc::from([original.clone(), decoded.clone()]);
        let mut fields = Vec::with_capacity(self.slots.len());
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(self.slots.len());
        for &(kind, i) in &self.slots {
            match kind {
                Kind::Str => {
                    let col = &mut self.strs[i];
                    if col.count == 0 {
                        continue;
                    }
                    let (views, nulls) = col.take(num_rows);
                    debug_assert!(views.iter().all(|v| {
                        let len = (*v as u32) as usize;
                        if len <= 12 {
                            return true;
                        }
                        let block = (*v >> 64) as u32;
                        let off = (*v >> 96) as usize;
                        let b = if block == 0 {
                            original.as_slice()
                        } else {
                            decoded.as_slice()
                        };
                        b.get(off..off + len)
                            .is_some_and(|s| std::str::from_utf8(s).is_ok())
                    }));
                    // Trusted-path exception, same as `columnar::accumulator`
                    // (re-validation costs ~1/3 of build time). Block 0 is the
                    // body, validated as UTF-8 as a whole in `scan`; block 1 is
                    // built from it by replacing ASCII escapes.
                    // SAFETY: every view comes from `make_view` over an
                    // in-bounds subslice of block 0 or 1 (both valid UTF-8);
                    // `views.len() == num_rows` and `nulls` has `num_rows` bits.
                    let array = unsafe {
                        StringViewArray::new_unchecked(
                            ScalarBuffer::from(views),
                            Arc::clone(&blocks),
                            nulls,
                        )
                    };
                    fields.push(Field::new(col.name.as_str(), DataType::Utf8View, true));
                    arrays.push(Arc::new(array));
                }
                Kind::F64 => {
                    let col = &mut self.f64s[i];
                    if col.count == 0 {
                        continue;
                    }
                    let name = col.name.clone();
                    let (values, nulls) = col.take(num_rows);
                    fields.push(Field::new(name, DataType::Float64, true));
                    arrays.push(Arc::new(Float64Array::new(values.into(), nulls)));
                }
                Kind::I64 => {
                    let col = &mut self.i64s[i];
                    if col.count == 0 {
                        continue;
                    }
                    let name = col.name.clone();
                    let (values, nulls) = col.take(num_rows);
                    fields.push(Field::new(name, DataType::Int64, true));
                    arrays.push(Arc::new(Int64Array::new(values.into(), nulls)));
                }
                Kind::Drop => {}
            }
        }
        let opts = RecordBatchOptions::new().with_row_count(Some(num_rows));
        RecordBatch::try_new_with_options(Arc::new(Schema::new(fields)), arrays, &opts)
    }
}

impl ScanBuilder for PromColumns {
    #[inline(always)]
    fn begin_row(&mut self) {}
    #[inline(always)]
    fn end_row(&mut self) {
        self.row += 1;
    }
    fn resolve_field(&mut self, key: &[u8]) -> usize {
        if let Some(&idx) = self.index.get(key) {
            return idx;
        }
        let is_synthetic = matches!(
            key,
            PROM_NAME_FIELD | PROM_VALUE_FIELD | PROM_TIMESTAMP_FIELD | PROM_TYPE_FIELD
        );
        if !is_synthetic {
            if self.labels >= self.max_labels {
                if let Some(idx) = self.drop_slot {
                    return idx;
                }
                let idx = self.slots.len();
                self.slots.push((Kind::Drop, 0));
                self.drop_slot = Some(idx);
                return idx;
            }
            self.labels += 1;
        }
        let name = String::from_utf8_lossy(key).into_owned();
        let kind = match key {
            PROM_VALUE_FIELD => Kind::F64,
            PROM_TIMESTAMP_FIELD => Kind::I64,
            _ => Kind::Str,
        };
        let pos = match kind {
            Kind::Str => {
                self.strs.push(Column::new(name));
                self.strs.len() - 1
            }
            Kind::F64 => {
                self.f64s.push(Column::new(name));
                self.f64s.len() - 1
            }
            Kind::I64 | Kind::Drop => {
                self.i64s.push(Column::new(name));
                self.i64s.len() - 1
            }
        };
        let idx = self.slots.len();
        self.slots.push((kind, pos));
        self.index.insert(key.to_vec(), idx);
        idx
    }
    #[inline(always)]
    #[allow(clippy::indexing_slicing)]
    fn append_str_by_idx(&mut self, idx: usize, value: &[u8]) {
        let (kind, i) = self.slot(idx);
        if kind != Kind::Str {
            return;
        }
        if self.strs[i].values.len() > self.row {
            self.has_duplicate = true;
            return;
        }
        // `value` is a subslice of `buf` (the scanner only hands out spans of
        // its input); the offset fits in u32 because `scan` rejects >4 GiB.
        let offset = (value.as_ptr() as usize).wrapping_sub(self.buf.as_ptr() as usize);
        debug_assert!(offset + value.len() <= self.buf.len());
        let view = make_view(value, 0, offset as u32);
        self.strs[i].set(self.row, view, &mut self.pad);
    }
    #[inline(always)]
    #[allow(clippy::indexing_slicing)]
    fn append_decoded_str_by_idx(&mut self, idx: usize, value: &[u8]) {
        let (kind, i) = self.slot(idx);
        if kind != Kind::Str {
            return;
        }
        if self.strs[i].values.len() > self.row {
            self.has_duplicate = true;
            return;
        }
        let Ok(offset) = u32::try_from(self.decoded.len()) else {
            return;
        };
        self.decoded.extend_from_slice(value);
        let view = make_view(value, 1, offset);
        self.strs[i].set(self.row, view, &mut self.pad);
    }
    fn append_int_by_idx(&mut self, _idx: usize, _value: &[u8]) {}
    fn append_float_by_idx(&mut self, _idx: usize, _value: &[u8]) {}
    fn append_bool_by_idx(&mut self, _idx: usize, _value: bool) {}
    fn append_null_by_idx(&mut self, _idx: usize) {}
    fn append_line(&mut self, _line: &[u8]) {}
}

impl PromBuilder for PromColumns {
    #[inline(always)]
    #[allow(clippy::indexing_slicing)]
    fn append_f64_by_idx(&mut self, idx: usize, value: f64) {
        if let (Kind::F64, i) = self.slot(idx) {
            self.f64s[i].set(self.row, value, &mut self.pad);
        }
    }
    #[inline(always)]
    #[allow(clippy::indexing_slicing)]
    fn append_i64_by_idx(&mut self, idx: usize, value: i64) {
        if let (Kind::I64, i) = self.slot(idx) {
            self.i64s[i].set(self.row, value, &mut self.pad);
        }
    }
}

/// Reusable Prometheus text → Arrow converter. Keep one per scrape target (or
/// per worker) so scratch buffers are reused across scrapes.
pub struct PromBatchBuilder {
    columns: PromColumns,
    scratch: PromScratch,
    config: PromScanConfig,
}

impl Default for PromBatchBuilder {
    fn default() -> Self {
        Self::new(PromScanConfig::default())
    }
}

impl PromBatchBuilder {
    /// Create a converter with the given scan options.
    pub fn new(config: PromScanConfig) -> Self {
        Self {
            columns: PromColumns::new(),
            scratch: PromScratch::new(),
            config,
        }
    }

    /// Reject bodies with more than `limit` label columns (default
    /// [`DEFAULT_MAX_LABEL_COLUMNS`]). Every column counts except the
    /// synthetic `__name__`, `__value__`, `__timestamp_ms__` and `__type__`.
    /// The limit is enforced while scanning, so a label-heavy body stops
    /// allocating columns at the limit.
    #[must_use]
    pub fn with_max_label_columns(mut self, limit: usize) -> Self {
        self.columns.max_labels = limit;
        self
    }

    /// Reject bodies whose sparse columns would need more than `limit` null
    /// cells in the dense output (default [`DEFAULT_MAX_PADDED_CELLS`]).
    /// Bounds memory for bodies with many rarely-used labels, where rows ×
    /// columns far exceeds the body size. Every sparse column counts: label
    /// columns, and `__timestamp_ms__` / `__type__` when only some samples
    /// carry them.
    #[must_use]
    pub fn with_max_padded_cells(mut self, limit: usize) -> Self {
        self.columns.pad = PadBudget::new(limit);
        self
    }

    /// Scan options used by the next [`scan`](Self::scan), e.g. to switch
    /// OpenMetrics timestamp parsing per response.
    pub fn config_mut(&mut self) -> &mut PromScanConfig {
        &mut self.config
    }

    /// Parse one scrape body into a `RecordBatch` (one row per sample).
    ///
    /// Any malformed line rejects the whole body, matching Prometheus.
    pub fn scan(&mut self, body: Bytes) -> Result<(RecordBatch, PromScanStats), PromToArrowError> {
        if u32::try_from(body.len()).is_err() {
            return Err(PromToArrowError::TooLarge);
        }
        self.columns.begin_batch(body.clone());
        let result = self.scan_into_columns(&body);
        if result.is_err() {
            // Drop the partial batch and the body reference now rather than
            // at the next scrape.
            self.columns.begin_batch(Bytes::new());
        }
        result
    }

    fn scan_into_columns(
        &mut self,
        body: &Bytes,
    ) -> Result<(RecordBatch, PromScanStats), PromToArrowError> {
        // The scanner validates UTF-8 before emitting any row.
        let stats = scan_prom_text(body, &self.config, &mut self.scratch, &mut self.columns)
            .map_err(|e| match e.kind {
                PromErrorKind::InvalidUtf8 => PromToArrowError::InvalidUtf8,
                _ => PromToArrowError::Parse(e),
            })?;
        if self.columns.has_duplicate {
            return Err(PromToArrowError::DuplicateLabel);
        }
        if self.columns.is_over_limit() {
            return Err(PromToArrowError::TooManyLabelColumns {
                limit: self.columns.max_labels,
            });
        }
        if !self.columns.reserve_finish_padding() {
            return Err(PromToArrowError::TooManyPaddedCells {
                limit: self.columns.pad.limit,
            });
        }
        let batch = self.columns.finish().map_err(PromToArrowError::Arrow)?;
        Ok((batch, stats))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Array, AsArray};
    use arrow::datatypes::{Float64Type, Int64Type};

    fn scan(text: &str) -> RecordBatch {
        PromBatchBuilder::default()
            .scan(Bytes::from(text.to_string()))
            .expect("valid exposition")
            .0
    }

    #[test]
    fn builds_typed_sparse_columns() {
        let batch = scan(
            "# TYPE http_requests_total counter\n\
             http_requests_total{method=\"post\",code=\"200\"} 1027 1395066363000\n\
             up 1\n\
             esc{path=\"C:\\\\x\"} 2.5\n",
        );
        assert_eq!(batch.num_rows(), 3);
        let schema = batch.schema();
        assert_eq!(
            schema
                .field_with_name("__name__")
                .expect("name")
                .data_type(),
            &DataType::Utf8View
        );
        let names = batch
            .column_by_name("__name__")
            .expect("name")
            .as_string_view();
        assert_eq!(names.value(0), "http_requests_total");
        assert_eq!(names.value(1), "up");
        let values = batch
            .column_by_name("__value__")
            .expect("value")
            .as_primitive::<Float64Type>();
        assert_eq!(values.values().as_ref(), &[1027.0, 1.0, 2.5]);
        let ts = batch
            .column_by_name("__timestamp_ms__")
            .expect("ts")
            .as_primitive::<Int64Type>();
        assert_eq!(ts.value(0), 1_395_066_363_000);
        assert!(ts.is_null(1));
        let method = batch
            .column_by_name("method")
            .expect("method")
            .as_string_view();
        assert_eq!(method.value(0), "post");
        assert!(method.is_null(1));
        let path = batch.column_by_name("path").expect("path").as_string_view();
        assert_eq!(path.value(2), "C:\\x");
        let ty = batch
            .column_by_name("__type__")
            .expect("type")
            .as_string_view();
        assert_eq!(ty.value(0), "counter");
        assert!(ty.is_null(1));
    }

    #[test]
    fn label_named_like_value_column_is_kept_as_string() {
        let batch = scan("m{__value__=\"raw\"} 2\n");
        let label = batch
            .column_by_name("label:__value__")
            .expect("prefixed label column")
            .as_string_view();
        assert_eq!(label.value(0), "raw");
        let values = batch
            .column_by_name("__value__")
            .expect("value")
            .as_primitive::<Float64Type>();
        assert_eq!(values.value(0), 2.0);
    }

    #[test]
    fn series_id_ignores_label_order_and_separates_series() {
        use arrow::datatypes::UInt64Type;
        let batch = scan(
            "m{a=\"1\",b=\"2\"} 1\n\
             m{b=\"2\",a=\"1\"} 2\n\
             m{a=\"2\",b=\"1\"} 3\n\
             m{a=\"1\"} 4\n\
             n{a=\"1\",b=\"2\"} 5\n",
        );
        let with_id = append_series_id(&batch).expect("series id");
        let ids = with_id
            .column_by_name(PROM_SERIES_ID_FIELD)
            .expect("id column")
            .as_primitive::<UInt64Type>();
        assert_eq!(ids.value(0), ids.value(1));
        for other in 2..5 {
            assert_ne!(ids.value(0), ids.value(other), "row {other}");
        }
        // Stable across batches with different column sets.
        let alone = append_series_id(&scan("m{b=\"2\",a=\"1\"} 9\n")).expect("series id");
        let alone_ids = alone
            .column_by_name(PROM_SERIES_ID_FIELD)
            .expect("id column")
            .as_primitive::<UInt64Type>();
        assert_eq!(alone_ids.value(0), ids.value(0));
    }

    #[test]
    fn rejects_malformed_and_recovers_for_next_scrape() {
        let mut b = PromBatchBuilder::default();
        assert!(matches!(
            b.scan(Bytes::from_static(b"ok 1\nbroken{\n")),
            Err(PromToArrowError::Parse(_))
        ));
        assert!(matches!(
            b.scan(Bytes::from_static(b"x{a=\"\xff\"} 1\n")),
            Err(PromToArrowError::InvalidUtf8)
        ));
        let (batch, stats) = b.scan(Bytes::from_static(b"ok 1\n")).expect("valid");
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(stats.samples, 1);
    }

    #[test]
    fn label_column_limit_is_enforced_while_scanning() {
        let mut b = PromBatchBuilder::default().with_max_label_columns(2);
        let err = b
            .scan(Bytes::from_static(
                b"m{a=\"1\",b=\"2\",c=\"3\",d=\"4\"} 1\n",
            ))
            .expect_err("too many labels");
        assert!(matches!(
            err,
            PromToArrowError::TooManyLabelColumns { limit: 2 }
        ));
        // The builder recovers, and the synthetic columns do not count.
        let (batch, _) = b
            .scan(Bytes::from_static(
                b"# TYPE m gauge\nm{a=\"1\",b=\"2\"} 1 5\n",
            ))
            .expect("within limit");
        assert_eq!(batch.num_columns(), 6);
        // Exposition labels starting with `__` do count.
        let err = b
            .scan(Bytes::from_static(b"m{__a=\"1\",__b=\"2\",__c=\"3\"} 1\n"))
            .expect_err("too many labels");
        assert!(matches!(
            err,
            PromToArrowError::TooManyLabelColumns { limit: 2 }
        ));
    }

    #[test]
    fn repeated_label_in_one_sample_is_rejected() {
        let mut b = PromBatchBuilder::default();
        assert!(matches!(
            b.scan(Bytes::from_static(b"m{zone=\"a\",zone=\"b\"} 1\n")),
            Err(PromToArrowError::DuplicateLabel)
        ));
        assert!(matches!(
            b.scan(Bytes::from_static(b"m{\"zone\"=\"a\",zone=\"b\\n\"} 1\n")),
            Err(PromToArrowError::DuplicateLabel)
        ));
        // The same label on consecutive samples is fine.
        let (batch, _) = b
            .scan(Bytes::from_static(b"m{zone=\"a\"} 1\nm{zone=\"b\"} 2\n"))
            .expect("valid");
        assert_eq!(batch.num_rows(), 2);
    }

    #[test]
    fn builder_keeps_no_reference_to_the_body() {
        let body = Bytes::from(b"m{zone=\"a-long-label-value\"} 1\n".to_vec());
        let mut b = PromBatchBuilder::default();
        let (batch, _) = b.scan(body.clone()).expect("valid");
        drop(batch);
        assert!(body.is_unique(), "the builder still holds the body");
    }

    #[test]
    fn padded_cell_budget_bounds_sparse_output() {
        // 4 rows; label `x` first appears on the last row (3 padded cells
        // during the scan), `y` only on the first (3 more at finish).
        let body = b"m{y=\"1\"} 1\nm 2\nm 3\nm{x=\"1\"} 4\n";
        let mut b = PromBatchBuilder::default().with_max_padded_cells(5);
        assert!(matches!(
            b.scan(Bytes::from_static(body)),
            Err(PromToArrowError::TooManyPaddedCells { limit: 5 })
        ));
        let mut b = PromBatchBuilder::default().with_max_padded_cells(6);
        let (batch, _) = b.scan(Bytes::from_static(body)).expect("fits");
        assert_eq!(batch.num_rows(), 4);
        // The budget is per body.
        assert!(b.scan(Bytes::from_static(body)).is_ok());
    }

    #[test]
    fn series_id_label_from_exposition_is_kept() {
        let batch = scan("m{__series_id__=\"7\"} 1\n");
        let batch = append_series_id(&batch).expect("series id");
        let kept = batch
            .column_by_name("label:__series_id__")
            .expect("prefixed label")
            .as_string_view();
        assert_eq!(kept.value(0), "7");
        assert_eq!(
            batch
                .column_by_name(PROM_SERIES_ID_FIELD)
                .expect("id")
                .data_type(),
            &DataType::UInt64
        );
        assert_eq!(
            PROM_SERIES_ID_FIELD.as_bytes(),
            ffwd_core::prom_text::PROM_SERIES_ID_FIELD
        );
    }

    #[test]
    fn series_id_ignores_empty_labels_and_keeps_metadata() {
        let a = scan("m{a=\"\"} 1\n");
        let b = scan("m 1\n");
        let id = |batch: &RecordBatch| {
            append_series_id(batch)
                .expect("id")
                .column_by_name(PROM_SERIES_ID_FIELD)
                .expect("id")
                .as_primitive::<arrow::datatypes::UInt64Type>()
                .value(0)
        };
        assert_eq!(id(&a), id(&b));
        let meta = std::collections::HashMap::from([("k".to_string(), "v".to_string())]);
        let with_meta = b
            .clone()
            .with_schema(Arc::new(b.schema().as_ref().clone().with_metadata(meta)))
            .expect("schema");
        let out = append_series_id(&with_meta).expect("id");
        assert_eq!(
            out.schema().metadata().get("k").map(String::as_str),
            Some("v")
        );
    }
}
