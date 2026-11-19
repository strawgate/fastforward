//! Start times for cumulative series (counters, histograms and summaries).
//!
//! Each target keeps a [`StartTimeTracker`], which adds the
//! [`PROM_START_TIMESTAMP_FIELD`] column to every scrape. It follows the
//! OpenTelemetry Prometheus compatibility spec:
//!
//! 1. An OpenMetrics `<family>_created` sample (`__type__ = created`) in the
//!    same scrape gives the start time of every sample of that family with
//!    the same labels (`le` and `quantile` aside).
//! 2. Otherwise the first scrape that sees a series sets its start time to
//!    the sample's own time, as the OpenTelemetry data model prescribes for
//!    cumulative streams with an unknown start. Later scrapes reuse it.
//! 3. When a counter, a histogram bucket or `_count`, or a summary `_count`
//!    goes down, the process behind it restarted after the previous
//!    observation, so the start time moves to that observation's time.
//!
//! Series missing from a scrape are forgotten, as Prometheus marks them
//! stale. At most `max_series` series are remembered per target; beyond
//! that, new series get their own time as start time on every scrape.
//! Gauges, untyped and info samples and `_created` samples themselves get a
//! null start time.

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray, Int64Array, StringViewArray};
use arrow::datatypes::{DataType, Field, FieldRef, Float64Type, Int64Type, Schema};
use arrow::error::ArrowError;
use arrow::record_batch::{RecordBatch, RecordBatchOptions};
use ffwd_arrow::prom::PROM_START_TIMESTAMP_FIELD;
use xxhash_rust::xxh64::xxh64;

/// Default limit on series remembered per target (about 40 bytes each).
pub const DEFAULT_MAX_TRACKED_SERIES: usize = 1 << 20;

const SEED: u64 = 0x7374_6172_745f_7473;

/// What a row is to start-time tracking.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Not cumulative: no start time.
    None,
    /// A `_created` sample: the start time of its family's series.
    Created,
    /// Cumulative; a decrease means a reset.
    Monotonic,
    /// Part of a cumulative family but may decrease (`_sum`, quantiles).
    Other,
}

struct SeriesState {
    start_ms: i64,
    last_ms: i64,
    last_value: f64,
    generation: u32,
}

/// Per-target start-time state. See the module docs.
pub struct StartTimeTracker {
    series: HashMap<u64, SeriesState>,
    generation: u32,
    max_series: usize,
    created: HashMap<u64, i64>,
    families: Vec<u64>,
    keys: Vec<u64>,
    roles: Vec<Role>,
}

impl StartTimeTracker {
    /// A tracker remembering at most `max_series` series.
    pub fn new(max_series: usize) -> Self {
        Self {
            series: HashMap::new(),
            generation: 0,
            max_series,
            created: HashMap::new(),
            families: Vec::new(),
            keys: Vec::new(),
            roles: Vec::new(),
        }
    }

    /// Number of series currently remembered.
    #[cfg(test)]
    pub fn tracked_series(&self) -> usize {
        self.series.len()
    }

    /// Return `batch` with a start-time column for one scrape. `batch` must
    /// have `__timestamp_ms__` filled on every row, as `finish_batch` leaves
    /// it.
    pub fn apply(&mut self, batch: &RecordBatch) -> Result<RecordBatch, ArrowError> {
        let num_rows = batch.num_rows();
        self.generation = self.generation.wrapping_add(1);
        self.classify(batch)?;
        let values = batch
            .column_by_name("__value__")
            .and_then(|c| c.as_primitive_opt::<Float64Type>());
        let ts = batch
            .column_by_name("__timestamp_ms__")
            .and_then(|c| c.as_primitive_opt::<Int64Type>());
        let (Some(values), Some(ts)) = (values, ts) else {
            return append_column(batch, Int64Array::new_null(num_rows));
        };

        self.created.clear();
        for row in 0..num_rows {
            if self.roles.get(row) == Some(&Role::Created)
                && values.is_valid(row)
                && let (Some(&family), Some(ms)) =
                    (self.families.get(row), seconds_to_ms(values.value(row)))
            {
                self.created.insert(family, ms);
            }
        }

        let mut starts: Vec<Option<i64>> = Vec::with_capacity(num_rows);
        for row in 0..num_rows {
            let role = self.roles.get(row).copied().unwrap_or(Role::None);
            if matches!(role, Role::None | Role::Created) || ts.is_null(row) {
                starts.push(None);
                continue;
            }
            let t = ts.value(row);
            let v = if values.is_valid(row) {
                values.value(row)
            } else {
                f64::NAN
            };
            let created = self
                .families
                .get(row)
                .and_then(|f| self.created.get(f))
                .copied();
            let key = self.keys.get(row).copied().unwrap_or(0);
            let start = match self.series.get_mut(&key) {
                Some(s) => {
                    if let Some(c) = created {
                        s.start_ms = c;
                    } else if role == Role::Monotonic && v < s.last_value {
                        s.start_ms = if s.last_ms < t { s.last_ms } else { t };
                    }
                    s.last_ms = t;
                    s.last_value = v;
                    s.generation = self.generation;
                    s.start_ms
                }
                None => {
                    let start = created.unwrap_or(t);
                    if self.series.len() < self.max_series {
                        self.series.insert(
                            key,
                            SeriesState {
                                start_ms: start,
                                last_ms: t,
                                last_value: v,
                                generation: self.generation,
                            },
                        );
                    }
                    start
                }
            };
            starts.push(Some(start.min(t)));
        }
        let generation = self.generation;
        self.series.retain(|_, s| s.generation == generation);
        append_column(batch, Int64Array::from(starts))
    }

    /// Fill `roles`, `families` (the key shared by a family's series with
    /// the same labels) and `keys` (the series key) for every row.
    fn classify(&mut self, batch: &RecordBatch) -> Result<(), ArrowError> {
        let num_rows = batch.num_rows();
        self.roles.clear();
        self.roles.resize(num_rows, Role::None);
        self.families.clear();
        self.families.resize(num_rows, 0);
        self.keys.clear();
        self.keys.resize(num_rows, 0);
        let (Some(names), Some(types)) = (
            string_view(batch, "__name__")?,
            string_view(batch, "__type__")?,
        ) else {
            return Ok(());
        };

        // Sum of label hashes, with `le` and `quantile` kept apart because
        // they are part of the series but not of the family.
        let mut base = vec![0u64; num_rows];
        let mut le = vec![0u64; num_rows];
        let mut quantile = vec![0u64; num_rows];
        let schema = batch.schema();
        for (field, col) in schema.fields().iter().zip(batch.columns()) {
            let name = field.name().as_str();
            // `job` and `instance` are the same on every row of a target.
            if name.starts_with("__") || name == "job" || name == "instance" {
                continue;
            }
            let Some(col) = as_string_view(col)? else {
                continue;
            };
            let target = match name {
                "le" => &mut le,
                "quantile" => &mut quantile,
                _ => &mut base,
            };
            let seed = xxh64(name.as_bytes(), SEED);
            for (row, slot) in target.iter_mut().enumerate() {
                if col.is_valid(row) {
                    let v = col.value(row);
                    if !v.is_empty() {
                        let h = xxh64(v.as_bytes(), seed);
                        *slot = slot.wrapping_add(h.rotate_left(17) ^ h);
                    }
                }
            }
        }

        for row in 0..num_rows {
            if names.is_null(row) || types.is_null(row) {
                continue;
            }
            let name = names.value(row);
            let has_quantile = quantile.get(row).is_some_and(|&q| q != 0);
            let (family, role, folded) = classify_row(name, types.value(row), has_quantile);
            if role == Role::None {
                continue;
            }
            let labels = base.get(row).copied().unwrap_or(0);
            let le = le.get(row).copied().unwrap_or(0);
            let q = quantile.get(row).copied().unwrap_or(0);
            let family_labels = match folded {
                Folded::Le => labels.wrapping_add(q),
                Folded::Quantile => labels.wrapping_add(le),
                Folded::Nothing => labels.wrapping_add(le).wrapping_add(q),
            };
            if let Some(slot) = self.roles.get_mut(row) {
                *slot = role;
            }
            if let Some(slot) = self.families.get_mut(row) {
                *slot = xxh64(family.as_bytes(), SEED).wrapping_add(family_labels);
            }
            if let Some(slot) = self.keys.get_mut(row) {
                *slot = xxh64(name.as_bytes(), SEED ^ 1)
                    .wrapping_add(labels)
                    .wrapping_add(le)
                    .wrapping_add(q);
            }
        }
        Ok(())
    }
}

/// Which label a family folds into its points.
enum Folded {
    Le,
    Quantile,
    Nothing,
}

/// Family name, role and folded label of a sample, by declared type and
/// name suffix. Matches how the OTLP metrics encoder reassembles families.
fn classify_row<'a>(name: &'a str, ty: &str, has_quantile: bool) -> (&'a str, Role, Folded) {
    let strip = |suffix: &str| name.strip_suffix(suffix).filter(|f| !f.is_empty());
    if ty.eq_ignore_ascii_case("created") {
        return (
            strip("_created").unwrap_or(name),
            Role::Created,
            Folded::Nothing,
        );
    }
    if ty.eq_ignore_ascii_case("counter") {
        return (
            strip("_total").unwrap_or(name),
            Role::Monotonic,
            Folded::Nothing,
        );
    }
    if ty.eq_ignore_ascii_case("histogram") {
        if let Some(f) = strip("_bucket") {
            return (f, Role::Monotonic, Folded::Le);
        }
        if let Some(f) = strip("_count") {
            return (f, Role::Monotonic, Folded::Le);
        }
        if let Some(f) = strip("_sum") {
            return (f, Role::Other, Folded::Le);
        }
        return (name, Role::None, Folded::Nothing);
    }
    if ty.eq_ignore_ascii_case("summary") {
        if has_quantile {
            return (name, Role::Other, Folded::Quantile);
        }
        if let Some(f) = strip("_count") {
            return (f, Role::Monotonic, Folded::Quantile);
        }
        if let Some(f) = strip("_sum") {
            return (f, Role::Other, Folded::Quantile);
        }
    }
    (name, Role::None, Folded::Nothing)
}

/// OpenMetrics `_created` values are seconds since the epoch.
fn seconds_to_ms(seconds: f64) -> Option<i64> {
    let ms = (seconds * 1000.0).round();
    // i64::MAX as f64 rounds up, so compare strictly.
    (ms.is_finite() && ms >= 0.0 && ms < i64::MAX as f64).then_some(ms as i64)
}

fn string_view(batch: &RecordBatch, name: &str) -> Result<Option<StringViewArray>, ArrowError> {
    match batch.column_by_name(name) {
        Some(col) => as_string_view(col),
        None => Ok(None),
    }
}

/// The column as `Utf8View`, or `None` when it is not a string column.
fn as_string_view(col: &ArrayRef) -> Result<Option<StringViewArray>, ArrowError> {
    match col.data_type() {
        DataType::Utf8View => Ok(Some(col.as_string_view().clone())),
        DataType::Utf8 | DataType::LargeUtf8 => {
            let cast = arrow::compute::cast(col, &DataType::Utf8View)?;
            Ok(Some(cast.as_string_view().clone()))
        }
        _ => Ok(None),
    }
}

/// Append (or replace) the start-time column.
fn append_column(batch: &RecordBatch, starts: Int64Array) -> Result<RecordBatch, ArrowError> {
    let schema = batch.schema();
    let mut fields: Vec<FieldRef> = Vec::with_capacity(schema.fields().len() + 1);
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len() + 1);
    for (field, col) in schema.fields().iter().zip(batch.columns()) {
        if field.name() != PROM_START_TIMESTAMP_FIELD {
            fields.push(Arc::clone(field));
            arrays.push(Arc::clone(col));
        }
    }
    fields.push(Arc::new(Field::new(
        PROM_START_TIMESTAMP_FIELD,
        DataType::Int64,
        true,
    )));
    arrays.push(Arc::new(starts));
    let opts = RecordBatchOptions::new().with_row_count(Some(batch.num_rows()));
    let schema = Schema::new_with_metadata(fields, schema.metadata().clone());
    RecordBatch::try_new_with_options(Arc::new(schema), arrays, &opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use ffwd_arrow::prom::PromBatchBuilder;
    use ffwd_core::prom_text::PromScanConfig;

    /// Scan `text` as one scrape at `scrape_ms` and apply `tracker`.
    fn scrape(
        tracker: &mut StartTimeTracker,
        text: &str,
        openmetrics: bool,
        scrape_ms: i64,
    ) -> Vec<(String, Option<i64>)> {
        let mut builder = PromBatchBuilder::new(PromScanConfig::default());
        builder.config_mut().openmetrics = openmetrics;
        let (batch, _) = builder
            .scan(Bytes::copy_from_slice(text.as_bytes()))
            .expect("valid exposition");
        let batch =
            super::super::finish_batch(&batch, "j", "i:1", scrape_ms, true).expect("finish");
        let out = tracker.apply(&batch).expect("apply");
        let names = out
            .column_by_name("__name__")
            .expect("name")
            .as_string_view();
        let starts = out
            .column_by_name(PROM_START_TIMESTAMP_FIELD)
            .expect("start")
            .as_primitive::<Int64Type>();
        (0..out.num_rows())
            .map(|r| {
                (
                    names.value(r).to_string(),
                    starts.is_valid(r).then(|| starts.value(r)),
                )
            })
            .collect()
    }

    fn start_of(rows: &[(String, Option<i64>)], name: &str) -> Option<i64> {
        rows.iter().find(|(n, _)| n == name).and_then(|(_, s)| *s)
    }

    #[test]
    fn created_sample_sets_start_time_of_its_family() {
        let mut t = StartTimeTracker::new(16);
        let text = "# TYPE jobs counter\njobs_total{a=\"x\"} 3\njobs_created{a=\"x\"} 1.5\n\
                    jobs_total{a=\"y\"} 4\n\
                    # TYPE lat histogram\nlat_bucket{le=\"1\"} 1\nlat_bucket{le=\"+Inf\"} 2\n\
                    lat_count 2\nlat_sum 3\nlat_created 2.25\n\
                    # TYPE rpc summary\nrpc{quantile=\"0.5\"} 1\nrpc_count 1\nrpc_sum 1\nrpc_created 3\n\
                    # TYPE g gauge\ng 1\n# EOF\n";
        let rows = scrape(&mut t, text, true, 10_000);
        assert_eq!(rows[0], ("jobs_total".into(), Some(1500)));
        // Same family, other labels: no `_created`, first seen now.
        assert_eq!(rows[2], ("jobs_total".into(), Some(10_000)));
        for name in ["lat_bucket", "lat_count", "lat_sum"] {
            assert_eq!(start_of(&rows, name), Some(2250), "{name}");
        }
        for name in ["rpc", "rpc_count", "rpc_sum"] {
            assert_eq!(start_of(&rows, name), Some(3000), "{name}");
        }
        assert_eq!(start_of(&rows, "jobs_created"), None);
        assert_eq!(start_of(&rows, "g"), None);
    }

    #[test]
    fn first_seen_time_is_kept_until_a_reset() {
        let mut t = StartTimeTracker::new(16);
        let c = |v: u32| format!("# TYPE reqs counter\nreqs_total {v}\n");
        assert_eq!(scrape(&mut t, &c(5), false, 1000)[0].1, Some(1000));
        assert_eq!(scrape(&mut t, &c(9), false, 2000)[0].1, Some(1000));
        // Decrease: the process restarted after the 2000 observation.
        assert_eq!(scrape(&mut t, &c(2), false, 3000)[0].1, Some(2000));
        assert_eq!(scrape(&mut t, &c(4), false, 4000)[0].1, Some(2000));
    }

    #[test]
    fn histogram_reset_is_detected_from_count_but_not_sum() {
        let mut t = StartTimeTracker::new(16);
        let h = |count: u32, sum: i32| {
            format!(
                "# TYPE lat histogram\nlat_bucket{{le=\"+Inf\"}} {count}\nlat_count {count}\nlat_sum {sum}\n"
            )
        };
        scrape(&mut t, &h(5, 10), false, 1000);
        // `_sum` going down is not a reset (negative observations).
        let rows = scrape(&mut t, &h(6, -3), false, 2000);
        assert!(rows.iter().all(|(_, s)| *s == Some(1000)), "{rows:?}");
        let rows = scrape(&mut t, &h(1, -3), false, 3000);
        assert_eq!(start_of(&rows, "lat_count"), Some(2000));
        assert_eq!(start_of(&rows, "lat_bucket"), Some(2000));
    }

    #[test]
    fn series_missing_from_a_scrape_are_forgotten() {
        let mut t = StartTimeTracker::new(16);
        let text = "# TYPE a counter\na 1\n# TYPE b counter\nb 1\n";
        scrape(&mut t, text, false, 1000);
        assert_eq!(t.tracked_series(), 2);
        scrape(&mut t, "# TYPE a counter\na 2\n", false, 2000);
        assert_eq!(t.tracked_series(), 1);
        // `b` comes back: a new series as far as start times go.
        let rows = scrape(
            &mut t,
            "# TYPE a counter\na 3\n# TYPE b counter\nb 1\n",
            false,
            3000,
        );
        assert_eq!(start_of(&rows, "a"), Some(1000));
        assert_eq!(start_of(&rows, "b"), Some(3000));
    }

    #[test]
    fn tracked_series_are_bounded() {
        let mut t = StartTimeTracker::new(2);
        let text = "# TYPE a counter\na 1\n# TYPE b counter\nb 1\n# TYPE c counter\nc 1\n";
        scrape(&mut t, text, false, 1000);
        assert_eq!(t.tracked_series(), 2);
        let rows = scrape(&mut t, text, false, 2000);
        assert_eq!(start_of(&rows, "a"), Some(1000));
        assert_eq!(start_of(&rows, "b"), Some(1000));
        // Not tracked: its start time is unknown, so it is the sample time.
        assert_eq!(start_of(&rows, "c"), Some(2000));
        assert_eq!(t.tracked_series(), 2);
    }

    #[test]
    fn start_time_never_follows_the_sample_time() {
        let mut t = StartTimeTracker::new(16);
        // A `_created` in the future (clock skew) is clamped.
        let text = "# TYPE x counter\nx_total 1\nx_created 99\n# EOF\n";
        let rows = scrape(&mut t, text, true, 5000);
        assert_eq!(rows[0].1, Some(5000));
    }

    #[test]
    fn untyped_and_prometheus_text_created_series_have_no_start() {
        let mut t = StartTimeTracker::new(16);
        let rows = scrape(
            &mut t,
            "foo 1\n# TYPE foo_total counter\nfoo_total 3\nfoo_created 4\n",
            false,
            1000,
        );
        assert_eq!(start_of(&rows, "foo"), None);
        assert_eq!(start_of(&rows, "foo_total"), Some(1000));
        assert_eq!(start_of(&rows, "foo_created"), None);
    }

    #[test]
    fn seconds_convert_to_milliseconds() {
        assert_eq!(seconds_to_ms(1.7e9), Some(1_700_000_000_000));
        assert_eq!(seconds_to_ms(1.2345), Some(1235));
        assert_eq!(seconds_to_ms(-1.0), None);
        assert_eq!(seconds_to_ms(f64::NAN), None);
        assert_eq!(seconds_to_ms(f64::INFINITY), None);
        assert_eq!(seconds_to_ms(1e300), None);
    }
}
