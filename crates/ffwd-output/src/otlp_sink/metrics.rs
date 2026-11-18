//! OTLP metrics encoding for Prometheus-shaped batches.
//!
//! A batch is metric-shaped when it has a string `__name__` column and a
//! numeric `__value__` column, which is what the `prometheus_scrape` input
//! produces (and what a SQL transform can produce from logs). Each row is one
//! sample; this module regroups samples into an `ExportMetricsServiceRequest`
//! following the OpenTelemetry Prometheus compatibility spec:
//!
//! - `job` → resource `service.name`, `instance` → `service.instance.id`.
//! - `__type__ = counter` → monotonic cumulative `Sum`.
//! - `gauge`, `unknown`/untyped, `info`, `stateset`, `gaugehistogram` and
//!   samples without a type → `Gauge`.
//! - `histogram` → cumulative `Histogram`, rebuilt from the `_bucket`
//!   (`le`), `_sum` and `_count` samples of each series.
//! - `summary` → `Summary`, rebuilt from the `quantile`, `_sum` and `_count`
//!   samples of each series.
//! - Other label columns become data point attributes. Columns whose names
//!   start with `__` are internal and are not exported. The `label:` prefix
//!   the scanner adds to labels that collide with its own columns is removed.
//!
//! Metric names are kept as scraped (no unit or `_total` suffix trimming), so
//! a round trip back to Prometheus keeps the original names. Start times are
//! not tracked yet; cumulative points carry `start_time_unix_nano = 0`.

use std::collections::HashMap;

use arrow::array::{Array, ArrayRef, AsArray};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Float64Type, Int64Type};
use arrow::record_batch::RecordBatch;
use ffwd_core::otlp::{encode_fixed64, encode_tag, encode_varint, encode_varint_field};
use ffwd_core::prom_text::parse_prom_value;

use super::types::{SCOPE_NAME, SCOPE_VERSION};

// ExportMetricsServiceRequest / ResourceMetrics / ScopeMetrics
const REQUEST_RESOURCE_METRICS: u32 = 1;
const RESOURCE_METRICS_RESOURCE: u32 = 1;
const RESOURCE_METRICS_SCOPE_METRICS: u32 = 2;
const RESOURCE_ATTRIBUTES: u32 = 1;
const SCOPE_METRICS_SCOPE: u32 = 1;
const SCOPE_METRICS_METRICS: u32 = 2;
const SCOPE_NAME_FIELD: u32 = 1;
const SCOPE_VERSION_FIELD: u32 = 2;
// Metric
const METRIC_NAME: u32 = 1;
const METRIC_GAUGE: u32 = 5;
const METRIC_SUM: u32 = 7;
const METRIC_HISTOGRAM: u32 = 9;
const METRIC_SUMMARY: u32 = 11;
// Gauge / Sum / Histogram / Summary
const DATA_POINTS: u32 = 1;
const AGGREGATION_TEMPORALITY: u32 = 2;
const SUM_IS_MONOTONIC: u32 = 3;
const TEMPORALITY_CUMULATIVE: u64 = 2;
// NumberDataPoint
const NDP_TIME: u32 = 3;
const NDP_AS_DOUBLE: u32 = 4;
const NDP_ATTRIBUTES: u32 = 7;
// HistogramDataPoint
const HDP_TIME: u32 = 3;
const HDP_COUNT: u32 = 4;
const HDP_SUM: u32 = 5;
const HDP_BUCKET_COUNTS: u32 = 6;
const HDP_EXPLICIT_BOUNDS: u32 = 7;
const HDP_ATTRIBUTES: u32 = 9;
// SummaryDataPoint
const SDP_TIME: u32 = 3;
const SDP_COUNT: u32 = 4;
const SDP_SUM: u32 = 5;
const SDP_QUANTILE_VALUES: u32 = 6;
const SDP_ATTRIBUTES: u32 = 7;
const VAQ_QUANTILE: u32 = 1;
const VAQ_VALUE: u32 = 2;
// KeyValue / AnyValue
const KV_KEY: u32 = 1;
const KV_VALUE: u32 = 2;
const ANY_STRING: u32 = 1;

const WIRE_LEN: u8 = 2;

const NAME_COL: &str = "__name__";
const VALUE_COL: &str = "__value__";
const TS_COL: &str = "__timestamp_ms__";
const TYPE_COL: &str = "__type__";
const RESERVED_LABEL_PREFIX: &str = "label:";

/// Returns true when `batch` has the columns needed to encode it as metrics.
pub(crate) fn is_metric_batch(batch: &RecordBatch) -> bool {
    let schema = batch.schema();
    let name_ok = schema
        .field_with_name(NAME_COL)
        .is_ok_and(|f| is_string_type(f.data_type()));
    let value_ok = schema
        .field_with_name(VALUE_COL)
        .is_ok_and(|f| f.data_type().is_numeric());
    name_ok && value_ok
}

fn is_string_type(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
    )
}

/// Borrowed string column of any Arrow string type.
enum StrCol<'a> {
    View(&'a arrow::array::StringViewArray),
    Utf8(&'a arrow::array::StringArray),
    Large(&'a arrow::array::LargeStringArray),
}

impl<'a> StrCol<'a> {
    fn new(col: &'a ArrayRef) -> Option<Self> {
        match col.data_type() {
            DataType::Utf8View => Some(Self::View(col.as_string_view())),
            DataType::Utf8 => Some(Self::Utf8(col.as_string::<i32>())),
            DataType::LargeUtf8 => Some(Self::Large(col.as_string::<i64>())),
            _ => None,
        }
    }

    #[inline]
    fn get(&self, row: usize) -> Option<&'a str> {
        match self {
            Self::View(a) => a.is_valid(row).then(|| a.value(row)),
            Self::Utf8(a) => a.is_valid(row).then(|| a.value(row)),
            Self::Large(a) => a.is_valid(row).then(|| a.value(row)),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Gauge,
    Counter,
    Histogram,
    Summary,
}

enum Role {
    Point,
    Bucket,
    Sum,
    Count,
    Quantile,
    Skip,
}

/// One histogram or summary series being reassembled.
struct Group {
    rep: usize,
    time_ms: i64,
    buckets: Vec<(f64, f64)>,
    quantiles: Vec<(f64, f64)>,
    sum: Option<f64>,
    count: Option<f64>,
}

struct Family<'a> {
    name: &'a str,
    kind: Kind,
    points: Vec<usize>,
    groups: Vec<Group>,
    group_index: HashMap<u64, Vec<usize>>,
}

struct Resource<'a> {
    job: Option<&'a str>,
    instance: Option<&'a str>,
    families: Vec<Family<'a>>,
    family_index: HashMap<(&'a str, Kind), usize>,
}

struct Columns<'a> {
    name: StrCol<'a>,
    values: &'a arrow::array::Float64Array,
    ts: Option<&'a arrow::array::Int64Array>,
    ty: Option<StrCol<'a>>,
    job: Option<StrCol<'a>>,
    instance: Option<StrCol<'a>>,
    labels: Vec<(&'a str, StrCol<'a>)>,
    le: Option<usize>,
    quantile: Option<usize>,
}

/// Encode `batch` as an `ExportMetricsServiceRequest` into `out`.
///
/// Returns the number of data points written. `default_time_ns` stamps rows
/// without a `__timestamp_ms__` value.
pub(crate) fn encode_metrics_batch(
    batch: &RecordBatch,
    default_time_ns: u64,
    out: &mut Vec<u8>,
) -> Result<usize, String> {
    out.clear();
    // Columns cast to the types the encoder reads. Kept alive for borrowing.
    let mut owned: Vec<(String, ArrayRef)> = Vec::new();
    let schema = batch.schema();
    for (field, col) in schema.fields().iter().zip(batch.columns()) {
        let name = field.name().as_str();
        let target = match name {
            VALUE_COL => Some(DataType::Float64),
            TS_COL => Some(DataType::Int64),
            _ if name.starts_with("__") && name != NAME_COL && name != TYPE_COL => continue,
            _ if is_string_type(col.data_type()) => None,
            _ => Some(DataType::Utf8),
        };
        let arr = match target {
            Some(dt) if col.data_type() != &dt => {
                cast(col, &dt).map_err(|e| format!("cast column '{name}': {e}"))?
            }
            _ => std::sync::Arc::clone(col),
        };
        owned.push((name.to_string(), arr));
    }
    let cols = resolve_columns(&owned)?;
    let resources = group_rows(&cols, batch.num_rows());
    let mut points = 0usize;
    let mut scratch = Scratch::default();
    for res in &resources {
        let start = begin(out);
        encode_resource_metrics(res, &cols, default_time_ns, &mut scratch, out, &mut points);
        end(out, REQUEST_RESOURCE_METRICS, start);
    }
    Ok(points)
}

fn resolve_columns(owned: &[(String, ArrayRef)]) -> Result<Columns<'_>, String> {
    let mut name = None;
    let mut values = None;
    let mut ts = None;
    let mut ty = None;
    let mut job = None;
    let mut instance = None;
    let mut labels = Vec::new();
    for (key, arr) in owned {
        match key.as_str() {
            NAME_COL => name = StrCol::new(arr),
            VALUE_COL => values = arr.as_primitive_opt::<Float64Type>(),
            TS_COL => ts = arr.as_primitive_opt::<Int64Type>(),
            TYPE_COL => ty = StrCol::new(arr),
            "job" => job = StrCol::new(arr),
            "instance" => instance = StrCol::new(arr),
            other => {
                if let Some(col) = StrCol::new(arr) {
                    let key = other.strip_prefix(RESERVED_LABEL_PREFIX).unwrap_or(other);
                    labels.push((key, col));
                }
            }
        }
    }
    let le = labels.iter().position(|(k, _)| *k == "le");
    let quantile = labels.iter().position(|(k, _)| *k == "quantile");
    Ok(Columns {
        name: name.ok_or("metric batch needs a string __name__ column")?,
        values: values.ok_or("metric batch needs a numeric __value__ column")?,
        ts,
        ty,
        job,
        instance,
        labels,
        le,
        quantile,
    })
}

fn classify<'a>(name: &'a str, ty: &str) -> (&'a str, Kind, Role) {
    let strip = |suffix: &str| name.strip_suffix(suffix).filter(|f| !f.is_empty());
    match ty {
        "counter" => (name, Kind::Counter, Role::Point),
        "histogram" => {
            if let Some(f) = strip("_bucket") {
                (f, Kind::Histogram, Role::Bucket)
            } else if let Some(f) = strip("_sum") {
                (f, Kind::Histogram, Role::Sum)
            } else if let Some(f) = strip("_count") {
                (f, Kind::Histogram, Role::Count)
            } else if let Some(f) = strip("_created") {
                (f, Kind::Histogram, Role::Skip)
            } else {
                (name, Kind::Gauge, Role::Point)
            }
        }
        "summary" => {
            if let Some(f) = strip("_sum") {
                (f, Kind::Summary, Role::Sum)
            } else if let Some(f) = strip("_count") {
                (f, Kind::Summary, Role::Count)
            } else if let Some(f) = strip("_created") {
                (f, Kind::Summary, Role::Skip)
            } else {
                (name, Kind::Summary, Role::Quantile)
            }
        }
        _ => (name, Kind::Gauge, Role::Point),
    }
}

/// Hash of a row's labels, leaving out `le` and `quantile`.
fn series_hash(cols: &Columns<'_>, row: usize) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for (i, (_, col)) in cols.labels.iter().enumerate() {
        if Some(i) == cols.le || Some(i) == cols.quantile {
            continue;
        }
        if let Some(v) = col.get(row) {
            h ^= i as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
            for b in v.bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            h ^= 0xff;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

fn same_series(cols: &Columns<'_>, a: usize, b: usize) -> bool {
    cols.labels.iter().enumerate().all(|(i, (_, col))| {
        Some(i) == cols.le || Some(i) == cols.quantile || col.get(a) == col.get(b)
    })
}

fn group_rows<'a>(cols: &Columns<'a>, num_rows: usize) -> Vec<Resource<'a>> {
    let mut resources: Vec<Resource<'a>> = Vec::new();
    let mut res_index: HashMap<(Option<&'a str>, Option<&'a str>), usize> = HashMap::new();
    for row in 0..num_rows {
        let Some(name) = cols.name.get(row) else {
            continue;
        };
        if cols.values.is_null(row) {
            continue;
        }
        let ty = cols.ty.as_ref().and_then(|c| c.get(row)).unwrap_or("");
        let (family_name, kind, role) = classify(name, ty);
        if matches!(role, Role::Skip) {
            continue;
        }
        let job = cols.job.as_ref().and_then(|c| c.get(row));
        let instance = cols.instance.as_ref().and_then(|c| c.get(row));
        let ri = *res_index.entry((job, instance)).or_insert_with(|| {
            resources.push(Resource {
                job,
                instance,
                families: Vec::new(),
                family_index: HashMap::new(),
            });
            resources.len() - 1
        });
        let res = &mut resources[ri];
        let fi = *res
            .family_index
            .entry((family_name, kind))
            .or_insert_with(|| {
                res.families.push(Family {
                    name: family_name,
                    kind,
                    points: Vec::new(),
                    groups: Vec::new(),
                    group_index: HashMap::new(),
                });
                res.families.len() - 1
            });
        let fam = &mut res.families[fi];
        let value = cols.values.value(row);
        let time_ms = cols
            .ts
            .and_then(|t| t.is_valid(row).then(|| t.value(row)))
            .unwrap_or(i64::MIN);
        match role {
            Role::Point => fam.points.push(row),
            Role::Bucket | Role::Sum | Role::Count | Role::Quantile => {
                let h = series_hash(cols, row);
                let candidates = fam.group_index.entry(h).or_default();
                let gi = match candidates
                    .iter()
                    .copied()
                    .find(|&g| same_series(cols, fam.groups[g].rep, row))
                {
                    Some(g) => g,
                    None => {
                        fam.groups.push(Group {
                            rep: row,
                            time_ms,
                            buckets: Vec::new(),
                            quantiles: Vec::new(),
                            sum: None,
                            count: None,
                        });
                        candidates.push(fam.groups.len() - 1);
                        fam.groups.len() - 1
                    }
                };
                let g = &mut fam.groups[gi];
                g.time_ms = g.time_ms.max(time_ms);
                let bound = |idx: Option<usize>| {
                    idx.and_then(|i| cols.labels[i].1.get(row))
                        .and_then(|s| parse_prom_value(s.as_bytes()))
                };
                match role {
                    Role::Bucket => {
                        if let Some(le) = bound(cols.le) {
                            g.buckets.push((le, value));
                        }
                    }
                    Role::Quantile => {
                        if let Some(q) = bound(cols.quantile) {
                            g.quantiles.push((q, value));
                        }
                    }
                    Role::Sum => g.sum = Some(value),
                    Role::Count => g.count = Some(value),
                    Role::Point | Role::Skip => {}
                }
            }
            Role::Skip => {}
        }
    }
    resources
}

#[derive(Default)]
struct Scratch {
    bounds: Vec<f64>,
    counts: Vec<u64>,
}

fn time_ns(time_ms: i64, default_ns: u64) -> u64 {
    if time_ms == i64::MIN {
        return default_ns;
    }
    u64::try_from(time_ms).map_or(0, |ms| ms.saturating_mul(1_000_000))
}

fn encode_resource_metrics(
    res: &Resource<'_>,
    cols: &Columns<'_>,
    default_ns: u64,
    scratch: &mut Scratch,
    out: &mut Vec<u8>,
    points: &mut usize,
) {
    let start = begin(out);
    if let Some(job) = res.job {
        encode_kv(out, RESOURCE_ATTRIBUTES, "service.name", job);
    }
    if let Some(instance) = res.instance {
        encode_kv(out, RESOURCE_ATTRIBUTES, "service.instance.id", instance);
    }
    end(out, RESOURCE_METRICS_RESOURCE, start);

    let scope_metrics = begin(out);
    let scope = begin(out);
    encode_str(out, SCOPE_NAME_FIELD, SCOPE_NAME);
    encode_str(out, SCOPE_VERSION_FIELD, SCOPE_VERSION);
    end(out, SCOPE_METRICS_SCOPE, scope);
    for fam in &res.families {
        let metric = begin(out);
        encode_str(out, METRIC_NAME, fam.name.as_bytes());
        match fam.kind {
            Kind::Gauge | Kind::Counter => {
                let data = begin(out);
                for &row in &fam.points {
                    let dp = begin(out);
                    let ms = cols
                        .ts
                        .and_then(|t| t.is_valid(row).then(|| t.value(row)))
                        .unwrap_or(i64::MIN);
                    encode_fixed64(out, NDP_TIME, time_ns(ms, default_ns));
                    encode_fixed64(out, NDP_AS_DOUBLE, cols.values.value(row).to_bits());
                    encode_attributes(out, NDP_ATTRIBUTES, cols, row);
                    end(out, DATA_POINTS, dp);
                    *points += 1;
                }
                if fam.kind == Kind::Counter {
                    encode_varint_field(out, AGGREGATION_TEMPORALITY, TEMPORALITY_CUMULATIVE);
                    encode_varint_field(out, SUM_IS_MONOTONIC, 1);
                    end(out, METRIC_SUM, data);
                } else {
                    end(out, METRIC_GAUGE, data);
                }
            }
            Kind::Histogram => {
                let data = begin(out);
                for g in &fam.groups {
                    encode_histogram_point(out, g, cols, default_ns, scratch);
                    *points += 1;
                }
                encode_varint_field(out, AGGREGATION_TEMPORALITY, TEMPORALITY_CUMULATIVE);
                end(out, METRIC_HISTOGRAM, data);
            }
            Kind::Summary => {
                let data = begin(out);
                for g in &fam.groups {
                    let dp = begin(out);
                    encode_fixed64(out, SDP_TIME, time_ns(g.time_ms, default_ns));
                    encode_fixed64(out, SDP_COUNT, float_count(g.count.unwrap_or(0.0)));
                    encode_fixed64(out, SDP_SUM, g.sum.unwrap_or(0.0).to_bits());
                    let mut quantiles = g.quantiles.clone();
                    quantiles.sort_by(|a, b| a.0.total_cmp(&b.0));
                    for (q, v) in quantiles {
                        let vaq = begin(out);
                        encode_fixed64(out, VAQ_QUANTILE, q.to_bits());
                        encode_fixed64(out, VAQ_VALUE, v.to_bits());
                        end(out, SDP_QUANTILE_VALUES, vaq);
                    }
                    encode_attributes(out, SDP_ATTRIBUTES, cols, g.rep);
                    end(out, DATA_POINTS, dp);
                    *points += 1;
                }
                end(out, METRIC_SUMMARY, data);
            }
        }
        end(out, SCOPE_METRICS_METRICS, metric);
    }
    end(out, RESOURCE_METRICS_SCOPE_METRICS, scope_metrics);
}

fn float_count(v: f64) -> u64 {
    if v.is_finite() && v > 0.0 {
        v.round() as u64
    } else {
        0
    }
}

fn encode_histogram_point(
    out: &mut Vec<u8>,
    g: &Group,
    cols: &Columns<'_>,
    default_ns: u64,
    scratch: &mut Scratch,
) {
    let mut buckets = g.buckets.clone();
    buckets.sort_by(|a, b| a.0.total_cmp(&b.0));
    scratch.bounds.clear();
    scratch.counts.clear();
    let mut prev = 0.0f64;
    let mut has_inf = false;
    for &(le, cum) in &buckets {
        let delta = float_count(cum - prev);
        prev = prev.max(cum);
        scratch.counts.push(delta);
        if le.is_infinite() && le > 0.0 {
            has_inf = true;
        } else {
            scratch.bounds.push(le);
        }
    }
    let count = g.count.unwrap_or(prev);
    if !has_inf {
        // Close the open-ended bucket from the total count.
        scratch.counts.push(float_count(count - prev));
    }

    let dp = begin(out);
    encode_fixed64(out, HDP_TIME, time_ns(g.time_ms, default_ns));
    encode_fixed64(out, HDP_COUNT, float_count(count));
    if let Some(sum) = g.sum {
        encode_fixed64(out, HDP_SUM, sum.to_bits());
    }
    if !scratch.counts.is_empty() {
        encode_tag(out, HDP_BUCKET_COUNTS, WIRE_LEN);
        encode_varint(out, (scratch.counts.len() * 8) as u64);
        for c in &scratch.counts {
            out.extend_from_slice(&c.to_le_bytes());
        }
    }
    if !scratch.bounds.is_empty() {
        encode_tag(out, HDP_EXPLICIT_BOUNDS, WIRE_LEN);
        encode_varint(out, (scratch.bounds.len() * 8) as u64);
        for b in &scratch.bounds {
            out.extend_from_slice(&b.to_bits().to_le_bytes());
        }
    }
    encode_attributes(out, HDP_ATTRIBUTES, cols, g.rep);
    end(out, DATA_POINTS, dp);
}

fn encode_attributes(out: &mut Vec<u8>, field: u32, cols: &Columns<'_>, row: usize) {
    for (i, (key, col)) in cols.labels.iter().enumerate() {
        if Some(i) == cols.le || Some(i) == cols.quantile {
            // `le` / `quantile` are folded into the point; for gauge rows
            // that carry them (untyped histograms), keep them as attributes.
            if cols.ty.is_some() && is_folded_row(cols, row) {
                continue;
            }
        }
        if let Some(v) = col.get(row) {
            encode_kv(out, field, key, v);
        }
    }
}

/// True when the row's `le`/`quantile` label was folded into a histogram or
/// summary point.
fn is_folded_row(cols: &Columns<'_>, row: usize) -> bool {
    let ty = cols.ty.as_ref().and_then(|c| c.get(row)).unwrap_or("");
    matches!(ty, "histogram" | "summary")
}

fn encode_kv(out: &mut Vec<u8>, field: u32, key: &str, value: &str) {
    let kv = begin(out);
    encode_str(out, KV_KEY, key.as_bytes());
    let any = begin(out);
    encode_str(out, ANY_STRING, value.as_bytes());
    end(out, KV_VALUE, any);
    end(out, field, kv);
}

fn encode_str(out: &mut Vec<u8>, field: u32, value: &[u8]) {
    encode_tag(out, field, WIRE_LEN);
    encode_varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

/// Start a length-delimited message; its body is written after the returned mark.
#[inline]
fn begin(out: &[u8]) -> usize {
    out.len()
}

/// Close a message opened with [`begin`]: insert its tag and length before the body.
fn end(out: &mut Vec<u8>, field: u32, start: usize) {
    let len = out.len() - start;
    let mut header = [0u8; 20];
    let mut n = put_varint(
        &mut header,
        0,
        (u64::from(field) << 3) | u64::from(WIRE_LEN),
    );
    n = put_varint(&mut header, n, len as u64);
    out.splice(start..start, header.iter().take(n).copied());
}

fn put_varint(buf: &mut [u8; 20], mut pos: usize, mut v: u64) -> usize {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if let Some(slot) = buf.get_mut(pos) {
            *slot = if v == 0 { byte } else { byte | 0x80 };
        }
        pos += 1;
        if v == 0 {
            return pos;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, Int64Array, StringArray};
    use arrow::datatypes::{Field, Schema};
    use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::metrics::v1::metric::Data;
    use prost::Message as _;
    use std::sync::Arc;

    type Row<'a> = (
        &'a str,
        f64,
        Option<i64>,
        Option<&'a str>,
        &'a [(&'a str, &'a str)],
    );

    fn batch(rows: &[Row<'_>]) -> RecordBatch {
        let mut label_names: Vec<&str> = Vec::new();
        for (_, _, _, _, labels) in rows {
            for (k, _) in *labels {
                if !label_names.contains(k) {
                    label_names.push(k);
                }
            }
        }
        let mut fields = vec![
            Field::new(NAME_COL, DataType::Utf8, true),
            Field::new(VALUE_COL, DataType::Float64, true),
            Field::new(TS_COL, DataType::Int64, true),
            Field::new(TYPE_COL, DataType::Utf8, true),
        ];
        let mut arrays: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from_iter(rows.iter().map(|r| Some(r.0)))),
            Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.1))),
            Arc::new(Int64Array::from_iter(rows.iter().map(|r| r.2))),
            Arc::new(StringArray::from_iter(rows.iter().map(|r| r.3))),
        ];
        for name in label_names {
            fields.push(Field::new(name, DataType::Utf8, true));
            arrays
                .push(Arc::new(StringArray::from_iter(rows.iter().map(|r| {
                    r.4.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
                }))));
        }
        RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("batch")
    }

    fn decode(b: &RecordBatch) -> ExportMetricsServiceRequest {
        let mut out = Vec::new();
        encode_metrics_batch(b, 99, &mut out).expect("encode");
        ExportMetricsServiceRequest::decode(out.as_slice()).expect("valid protobuf")
    }

    fn attr(attrs: &[opentelemetry_proto::tonic::common::v1::KeyValue], key: &str) -> String {
        attrs
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| match &v.value {
                Some(Value::StringValue(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn metrics_endpoint_follows_logs_endpoint() {
        use super::super::types::metrics_endpoint_for;
        use ffwd_config::OtlpProtocol;
        assert_eq!(
            metrics_endpoint_for("http://c:4318/v1/logs", OtlpProtocol::Http),
            "http://c:4318/v1/metrics"
        );
        assert_eq!(
            metrics_endpoint_for("http://c:9000/ingest", OtlpProtocol::Http),
            "http://c:9000/ingest"
        );
        assert_eq!(
            metrics_endpoint_for("http://c:4317", OtlpProtocol::Grpc),
            "http://c:4317/opentelemetry.proto.collector.metrics.v1.MetricsService/Export"
        );
        assert_eq!(
            metrics_endpoint_for(
                "http://c:4317/opentelemetry.proto.collector.logs.v1.LogsService/Export",
                OtlpProtocol::Grpc
            ),
            "http://c:4317/opentelemetry.proto.collector.metrics.v1.MetricsService/Export"
        );
    }

    #[test]
    fn metric_batch_detection() {
        let b = batch(&[("up", 1.0, None, None, &[])]);
        assert!(is_metric_batch(&b));
        let logs = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("body", DataType::Utf8, true)])),
            vec![Arc::new(StringArray::from(vec!["x"]))],
        )
        .expect("batch");
        assert!(!is_metric_batch(&logs));
    }

    #[test]
    fn gauges_and_counters_map_to_gauge_and_monotonic_sum() {
        let b = batch(&[
            (
                "temp",
                21.5,
                Some(1000),
                Some("gauge"),
                &[("job", "node"), ("instance", "h:1"), ("room", "a")],
            ),
            (
                "reqs_total",
                7.0,
                None,
                Some("counter"),
                &[("job", "node"), ("instance", "h:1")],
            ),
            (
                "untyped",
                3.0,
                None,
                None,
                &[("job", "node"), ("instance", "h:1")],
            ),
        ]);
        let req = decode(&b);
        assert_eq!(req.resource_metrics.len(), 1);
        let rm = &req.resource_metrics[0];
        let res = rm.resource.as_ref().expect("resource");
        assert_eq!(attr(&res.attributes, "service.name"), "node");
        assert_eq!(attr(&res.attributes, "service.instance.id"), "h:1");
        let metrics = &rm.scope_metrics[0].metrics;
        assert_eq!(metrics.len(), 3);
        let Some(Data::Gauge(g)) = &metrics[0].data else {
            panic!("temp should be a gauge")
        };
        assert_eq!(metrics[0].name, "temp");
        assert_eq!(g.data_points[0].time_unix_nano, 1_000_000_000);
        assert_eq!(attr(&g.data_points[0].attributes, "room"), "a");
        assert!(
            g.data_points[0].attributes.iter().all(|kv| kv.key != "job"),
            "job is a resource attribute"
        );
        let Some(Data::Sum(s)) = &metrics[1].data else {
            panic!("counter should be a sum")
        };
        assert!(s.is_monotonic);
        assert_eq!(s.aggregation_temporality, 2);
        assert_eq!(s.data_points[0].time_unix_nano, 99);
        assert!(matches!(metrics[2].data, Some(Data::Gauge(_))));
    }

    #[test]
    fn histogram_samples_are_reassembled_per_series() {
        let h = Some("histogram");
        let b = batch(&[
            (
                "lat_bucket",
                1.0,
                Some(5),
                h,
                &[("path", "/a"), ("le", "0.1")],
            ),
            (
                "lat_bucket",
                3.0,
                Some(5),
                h,
                &[("path", "/a"), ("le", "1")],
            ),
            (
                "lat_bucket",
                4.0,
                Some(5),
                h,
                &[("path", "/a"), ("le", "+Inf")],
            ),
            ("lat_sum", 2.5, Some(5), h, &[("path", "/a")]),
            ("lat_count", 4.0, Some(5), h, &[("path", "/a")]),
            (
                "lat_bucket",
                2.0,
                Some(5),
                h,
                &[("path", "/b"), ("le", "0.1")],
            ),
            (
                "lat_bucket",
                2.0,
                Some(5),
                h,
                &[("path", "/b"), ("le", "1")],
            ),
            (
                "lat_bucket",
                2.0,
                Some(5),
                h,
                &[("path", "/b"), ("le", "+Inf")],
            ),
            ("lat_count", 2.0, Some(5), h, &[("path", "/b")]),
        ]);
        let req = decode(&b);
        let metrics = &req.resource_metrics[0].scope_metrics[0].metrics;
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "lat");
        let Some(Data::Histogram(hist)) = &metrics[0].data else {
            panic!("expected histogram")
        };
        assert_eq!(hist.aggregation_temporality, 2);
        assert_eq!(hist.data_points.len(), 2);
        let a = &hist.data_points[0];
        assert_eq!(attr(&a.attributes, "path"), "/a");
        assert!(a.attributes.iter().all(|kv| kv.key != "le"));
        assert_eq!(a.explicit_bounds, vec![0.1, 1.0]);
        assert_eq!(a.bucket_counts, vec![1, 2, 1]);
        assert_eq!(a.count, 4);
        assert_eq!(a.sum, Some(2.5));
        let bpt = &hist.data_points[1];
        assert_eq!(bpt.bucket_counts, vec![2, 0, 0]);
        assert_eq!(bpt.sum, None);
    }

    #[test]
    fn summary_samples_are_reassembled() {
        let s = Some("summary");
        let b = batch(&[
            ("rpc", 0.2, None, s, &[("quantile", "0.99")]),
            ("rpc", 0.1, None, s, &[("quantile", "0.5")]),
            ("rpc_sum", 10.0, None, s, &[]),
            ("rpc_count", 50.0, None, s, &[]),
        ]);
        let req = decode(&b);
        let metrics = &req.resource_metrics[0].scope_metrics[0].metrics;
        let Some(Data::Summary(sum)) = &metrics[0].data else {
            panic!("expected summary")
        };
        let p = &sum.data_points[0];
        assert_eq!(p.count, 50);
        assert_eq!(p.sum, 10.0);
        let qs: Vec<(f64, f64)> = p
            .quantile_values
            .iter()
            .map(|q| (q.quantile, q.value))
            .collect();
        assert_eq!(qs, vec![(0.5, 0.1), (0.99, 0.2)]);
        assert!(p.attributes.is_empty());
    }

    #[test]
    fn untyped_le_label_stays_an_attribute() {
        let b = batch(&[("weird_bucket", 1.0, None, None, &[("le", "1")])]);
        let req = decode(&b);
        let metrics = &req.resource_metrics[0].scope_metrics[0].metrics;
        let Some(Data::Gauge(g)) = &metrics[0].data else {
            panic!("expected gauge")
        };
        assert_eq!(attr(&g.data_points[0].attributes, "le"), "1");
    }

    #[test]
    fn separate_targets_become_separate_resources_and_prefix_is_stripped() {
        let b = batch(&[
            (
                "up",
                1.0,
                None,
                None,
                &[("instance", "a:1"), ("label:__value__", "x")],
            ),
            ("up", 0.0, None, None, &[("instance", "b:1")]),
        ]);
        let req = decode(&b);
        assert_eq!(req.resource_metrics.len(), 2);
        let Some(Data::Gauge(g)) = &req.resource_metrics[0].scope_metrics[0].metrics[0].data else {
            panic!("expected gauge")
        };
        assert_eq!(attr(&g.data_points[0].attributes, "__value__"), "x");
    }
}
