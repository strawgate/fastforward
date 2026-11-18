//! OTLP metrics encoding for Prometheus-shaped batches.
//!
//! A batch is metric-shaped when it has a string `__name__` column and a
//! numeric `__value__` column, which is what the `prometheus_scrape` input
//! produces (and what a SQL transform can produce from logs). Each row is one
//! sample; this module regroups samples into an `ExportMetricsServiceRequest`
//! following the OpenTelemetry Prometheus compatibility spec:
//!
//! - `job` → resource `service.name`, `instance` → `service.instance.id`.
//! - `__type__` is matched case-insensitively.
//! - `__type__ = counter` → monotonic cumulative `Sum`.
//! - `__type__ = created` (OpenMetrics `_created` start-time samples) are
//!   not exported.
//! - `gauge`, `unknown`/untyped, `info`, `stateset`, `gaugehistogram` and
//!   samples without a type → `Gauge`.
//! - `histogram` → cumulative `Histogram`, rebuilt from the `_bucket`
//!   (`le`), `_sum` and `_count` samples of each series.
//! - `summary` → `Summary`, rebuilt from the `quantile`, `_sum` and `_count`
//!   samples of each series.
//!   Samples are grouped per label set *and* timestamp. A histogram without
//!   a count (no `_count` and no `+Inf` bucket), with decreasing buckets, or
//!   whose `+Inf` bucket disagrees with `_count` is dropped, as is a summary
//!   without `_count`.
//! - Other label columns become data point attributes; `le` is folded only
//!   into histogram points and `quantile` only into summary points, so on any
//!   other metric they stay attributes. Columns whose names start with `__`
//!   are internal and are not exported, and columns with no string form
//!   (structs, lists) are skipped. The `label:` prefix the scanner adds to
//!   labels that collide with its own columns is removed.
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
const SERIES_ID_COL: &str = "__series_id__";

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

/// Declared Prometheus type of a row, matched case-insensitively.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TypeTag {
    Counter,
    Histogram,
    Summary,
    /// An OpenMetrics `_created` (start-time) sample.
    Created,
    Other,
}

impl TypeTag {
    fn parse(ty: &str) -> Self {
        if ty.eq_ignore_ascii_case("counter") {
            Self::Counter
        } else if ty.eq_ignore_ascii_case("histogram") {
            Self::Histogram
        } else if ty.eq_ignore_ascii_case("summary") {
            Self::Summary
        } else if ty.eq_ignore_ascii_case("created") {
            Self::Created
        } else {
            Self::Other
        }
    }
}

enum Role {
    Point,
    Bucket,
    Sum,
    Count,
    Quantile,
    Skip,
}

/// One histogram or summary series at one timestamp being reassembled.
struct Group {
    rep: usize,
    time_ms: Option<i64>,
    /// `(le, cumulative count)` or `(quantile, value)`, sorted by `finalize`.
    entries: Vec<(f64, f64)>,
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
    /// Time for rows without a usable timestamp.
    default_ns: u64,
}

impl Columns<'_> {
    /// The time a row is encoded with. Points are grouped by this, so rows
    /// that encode to the same time belong to the same point.
    fn time_key(&self, row: usize) -> u64 {
        time_ns(self.time_ms(row), self.default_ns)
    }

    /// Label `i` of `row`. An empty value counts as absent, as in
    /// Prometheus, so `{a=""}` and `{}` are the same series.
    fn label(&self, i: usize, row: usize) -> Option<&str> {
        self.labels
            .get(i)
            .and_then(|(_, col)| col.get(row))
            .filter(|v| !v.is_empty())
    }

    /// The row's timestamp, or `None` when it has none or it can't be
    /// encoded (see [`time_ns`]). Grouping uses this effective value, so a
    /// missing and an unusable timestamp land in the same point.
    fn time_ms(&self, row: usize) -> Option<i64> {
        self.ts
            .and_then(|t| t.is_valid(row).then(|| t.value(row)))
            .filter(|&ms| ms_to_ns(ms).is_some())
    }

    fn type_tag(&self, row: usize) -> TypeTag {
        self.ty
            .as_ref()
            .and_then(|c| c.get(row))
            .map_or(TypeTag::Other, TypeTag::parse)
    }

    /// The label folded into points of `kind` (`le` or `quantile`), if any.
    fn folded_label(&self, kind: Kind) -> Option<usize> {
        match kind {
            Kind::Histogram => self.le,
            Kind::Summary => self.quantile,
            Kind::Gauge | Kind::Counter => None,
        }
    }
}

/// Encode `batch` as an `ExportMetricsServiceRequest` into `out`.
///
/// Returns the number of data points written. `default_time_ns` stamps rows
/// without a `__timestamp_ms__` value.
pub(crate) fn encode_metrics_batch(
    batch: &RecordBatch,
    resource_attrs: &[(String, String)],
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
            VALUE_COL if !col.data_type().is_numeric() => {
                return Err(format!(
                    "metric batch needs a numeric __value__ column, got {}",
                    col.data_type()
                ));
            }
            VALUE_COL => Some(DataType::Float64),
            TS_COL => Some(DataType::Int64),
            _ if name.starts_with("__") && name != NAME_COL && name != TYPE_COL => continue,
            _ if is_string_type(col.data_type()) => None,
            _ => Some(DataType::Utf8),
        };
        let arr = match target {
            Some(dt) if col.data_type() != &dt => match cast(col, &dt) {
                Ok(arr) => arr,
                // The value and timestamp columns must be usable.
                Err(e) if name == VALUE_COL || name == TS_COL => {
                    return Err(format!("cast column '{name}': {e}"));
                }
                // Extra columns that have no string form (structs, lists,
                // ...) are left out of the attributes.
                Err(_) => continue,
            },
            _ => std::sync::Arc::clone(col),
        };
        owned.push((name.to_string(), arr));
    }
    let mut cols = resolve_columns(&owned)?;
    cols.default_ns = default_time_ns;
    let mut resources = group_rows(&cols, batch.num_rows());
    let mut points = 0usize;
    let mut scratch = Scratch::default();
    for res in &mut resources {
        for fam in &mut res.families {
            let kind = fam.kind;
            fam.groups.retain_mut(|g| finalize_group(g, kind));
        }
        let start = begin(out);
        encode_resource_metrics(
            res,
            &cols,
            resource_attrs,
            default_time_ns,
            &mut scratch,
            out,
            &mut points,
        );
        end(out, REQUEST_RESOURCE_METRICS, start);
    }
    Ok(points)
}

/// Remove the label prefix the scanner adds to labels that collide with its
/// own columns. Other labels that happen to start with `label:` are kept.
fn unprefixed_label(key: &str) -> &str {
    match key.strip_prefix(RESERVED_LABEL_PREFIX) {
        Some(rest)
            if matches!(rest, VALUE_COL | TS_COL | TYPE_COL | SERIES_ID_COL)
                || rest.starts_with(RESERVED_LABEL_PREFIX) =>
        {
            rest
        }
        _ => key,
    }
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
                    labels.push((unprefixed_label(other), col));
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
        default_ns: 0,
    })
}

fn classify(name: &str, ty: TypeTag, has_quantile: bool) -> (&str, Kind, Role) {
    let strip = |suffix: &str| name.strip_suffix(suffix).filter(|f| !f.is_empty());
    match ty {
        TypeTag::Counter => (name, Kind::Counter, Role::Point),
        TypeTag::Histogram => {
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
        // A quantile sample keeps its name even when it ends in `_sum` or
        // `_count` (family `rpc_sum` has samples `rpc_sum{quantile=..}`).
        TypeTag::Summary if has_quantile => (name, Kind::Summary, Role::Quantile),
        TypeTag::Summary => {
            if let Some(f) = strip("_sum") {
                (f, Kind::Summary, Role::Sum)
            } else if let Some(f) = strip("_count") {
                (f, Kind::Summary, Role::Count)
            } else if let Some(f) = strip("_created") {
                (f, Kind::Summary, Role::Skip)
            } else {
                (name, Kind::Gauge, Role::Point)
            }
        }
        // Start times aren't exported as points (no start-time support yet).
        TypeTag::Created => (name, Kind::Gauge, Role::Skip),
        TypeTag::Other => (name, Kind::Gauge, Role::Point),
    }
}

/// Hash of a row's labels and timestamp, leaving out the `skip` label.
fn series_hash(cols: &Columns<'_>, row: usize, skip: Option<usize>) -> u64 {
    const PRIME: u64 = 0x0100_0000_01b3;
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for i in 0..cols.labels.len() {
        if Some(i) == skip {
            continue;
        }
        if let Some(v) = cols.label(i, row) {
            h ^= i as u64;
            h = h.wrapping_mul(PRIME);
            for b in v.bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(PRIME);
            }
            h ^= 0xff;
            h = h.wrapping_mul(PRIME);
        }
    }
    h ^= cols.time_key(row);
    h = h.wrapping_mul(PRIME);
    h
}

fn same_series(cols: &Columns<'_>, a: usize, b: usize, skip: Option<usize>) -> bool {
    cols.time_key(a) == cols.time_key(b)
        && (0..cols.labels.len()).all(|i| Some(i) == skip || cols.label(i, a) == cols.label(i, b))
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
        let has_quantile = cols
            .quantile
            .is_some_and(|i| cols.labels[i].1.get(row).is_some());
        let (family_name, kind, role) = classify(name, cols.type_tag(row), has_quantile);
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
        if matches!(role, Role::Point) {
            fam.points.push(row);
            continue;
        }
        let skip = cols.folded_label(kind);
        let candidates = fam
            .group_index
            .entry(series_hash(cols, row, skip))
            .or_default();
        let gi = match candidates
            .iter()
            .copied()
            .find(|&g| same_series(cols, fam.groups[g].rep, row, skip))
        {
            Some(g) => g,
            None => {
                fam.groups.push(Group {
                    rep: row,
                    time_ms: cols.time_ms(row),
                    entries: Vec::new(),
                    sum: None,
                    count: None,
                });
                candidates.push(fam.groups.len() - 1);
                fam.groups.len() - 1
            }
        };
        let g = &mut fam.groups[gi];
        let bound = skip
            .and_then(|i| cols.labels[i].1.get(row))
            .and_then(|s| parse_prom_value(s.as_bytes()));
        match role {
            Role::Bucket | Role::Quantile => {
                if let Some(b) = bound {
                    g.entries.push((b, value));
                }
            }
            Role::Sum => g.sum = Some(value),
            Role::Count => g.count = Some(value),
            Role::Point | Role::Skip => {}
        }
    }
    resources
}

/// Sort a reassembled group and check it is complete. Returns false for a
/// group that can't be encoded faithfully, which is then dropped:
/// - a histogram without a count (neither `_count` nor a `+Inf` bucket),
///   with a negative or non-finite count, with decreasing cumulative bucket
///   counts, or whose `+Inf` bucket disagrees with `_count`;
/// - a summary without a usable `_count`.
fn finalize_group(g: &mut Group, kind: Kind) -> bool {
    g.entries.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let is_count = |c: f64| c.is_finite() && c >= 0.0;
    if g.count.is_some_and(|c| !is_count(c)) {
        return false;
    }
    match kind {
        Kind::Histogram => {
            if g.entries.iter().any(|&(le, c)| le.is_nan() || !is_count(c)) {
                return false;
            }
            if g.entries.windows(2).any(|w| w[1].1 < w[0].1) {
                return false;
            }
            let inf = g
                .entries
                .last()
                .filter(|(le, _)| *le == f64::INFINITY)
                .map(|&(_, c)| c);
            let last = g.entries.last().map_or(0.0, |&(_, c)| c);
            match (g.count, inf) {
                (Some(count), Some(inf)) if float_count(count) != float_count(inf) => false,
                (Some(count), None) if count < last => false,
                (None, None) => false,
                (None, Some(inf)) => {
                    g.count = Some(inf);
                    true
                }
                _ => true,
            }
        }
        Kind::Summary => g.count.is_some(),
        Kind::Gauge | Kind::Counter => true,
    }
}

#[derive(Default)]
struct Scratch {
    bounds: Vec<f64>,
    counts: Vec<u64>,
}

/// Nanoseconds for a millisecond timestamp, or `default_ns` when the row has
/// none or it doesn't fit (negative or past year 2554).
fn time_ns(time_ms: Option<i64>, default_ns: u64) -> u64 {
    time_ms.and_then(ms_to_ns).unwrap_or(default_ns)
}

fn ms_to_ns(ms: i64) -> Option<u64> {
    u64::try_from(ms).ok()?.checked_mul(1_000_000)
}

fn encode_resource_metrics(
    res: &Resource<'_>,
    cols: &Columns<'_>,
    resource_attrs: &[(String, String)],
    default_ns: u64,
    scratch: &mut Scratch,
    out: &mut Vec<u8>,
    points: &mut usize,
) {
    let start = begin(out);
    // Configured resource attributes first. The scraped target's identity
    // (`job`, `instance`) wins over a configured key of the same name.
    for (k, v) in resource_attrs {
        let is_overridden = (k == "service.name" && res.job.is_some())
            || (k == "service.instance.id" && res.instance.is_some());
        if !is_overridden {
            encode_kv(out, RESOURCE_ATTRIBUTES, k, v);
        }
    }
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
        if fam.points.is_empty() && fam.groups.is_empty() {
            continue;
        }
        let metric = begin(out);
        encode_str(out, METRIC_NAME, fam.name.as_bytes());
        match fam.kind {
            Kind::Gauge | Kind::Counter => {
                let data = begin(out);
                for &row in &fam.points {
                    let dp = begin(out);
                    encode_fixed64(out, NDP_TIME, time_ns(cols.time_ms(row), default_ns));
                    encode_fixed64(out, NDP_AS_DOUBLE, cols.values.value(row).to_bits());
                    encode_attributes(out, NDP_ATTRIBUTES, cols, row, None);
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
                    for &(q, v) in &g.entries {
                        let vaq = begin(out);
                        encode_fixed64(out, VAQ_QUANTILE, q.to_bits());
                        encode_fixed64(out, VAQ_VALUE, v.to_bits());
                        end(out, SDP_QUANTILE_VALUES, vaq);
                    }
                    encode_attributes(out, SDP_ATTRIBUTES, cols, g.rep, cols.quantile);
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

/// Encode a histogram group that passed [`finalize_group`]: entries are
/// sorted, cumulative counts never decrease, and `count` is set.
fn encode_histogram_point(
    out: &mut Vec<u8>,
    g: &Group,
    cols: &Columns<'_>,
    default_ns: u64,
    scratch: &mut Scratch,
) {
    scratch.bounds.clear();
    scratch.counts.clear();
    // Round the cumulative counts, then take differences, so the buckets
    // always add up to the encoded total.
    let mut prev = 0u64;
    let mut has_inf = false;
    for &(le, cum) in &g.entries {
        let cum = float_count(cum);
        scratch.counts.push(cum.saturating_sub(prev));
        prev = cum;
        if le == f64::INFINITY {
            has_inf = true;
        } else {
            scratch.bounds.push(le);
        }
    }
    let count = g.count.map_or(prev, float_count);
    if !has_inf {
        // Close the open-ended bucket from the total count.
        scratch.counts.push(count.saturating_sub(prev));
    }

    let dp = begin(out);
    encode_fixed64(out, HDP_TIME, time_ns(g.time_ms, default_ns));
    encode_fixed64(out, HDP_COUNT, count);
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
    encode_attributes(out, HDP_ATTRIBUTES, cols, g.rep, cols.le);
    end(out, DATA_POINTS, dp);
}

/// Encode a row's labels as attributes, leaving out the `skip` label (the
/// `le` or `quantile` folded into a histogram or summary point).
fn encode_attributes(
    out: &mut Vec<u8>,
    field: u32,
    cols: &Columns<'_>,
    row: usize,
    skip: Option<usize>,
) {
    for (i, (key, _)) in cols.labels.iter().enumerate() {
        if Some(i) == skip {
            continue;
        }
        if let Some(v) = cols.label(i, row) {
            encode_kv(out, field, key, v);
        }
    }
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
        encode_metrics_batch(b, &[], 99, &mut out).expect("encode");
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
            metrics_endpoint_for("http://c:4318/v1/logs?tenant=a", OtlpProtocol::Http),
            "http://c:4318/v1/metrics?tenant=a"
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
    fn configured_resource_attributes_are_added_and_target_identity_wins() {
        let b = batch(&[(
            "up",
            1.0,
            None,
            Some("gauge"),
            &[("job", "node"), ("instance", "h:1")],
        )]);
        let attrs = [
            ("deployment.environment".to_string(), "prod".to_string()),
            ("service.name".to_string(), "configured".to_string()),
        ];
        let mut out = Vec::new();
        encode_metrics_batch(&b, &attrs, 0, &mut out).expect("encode");
        let req = ExportMetricsServiceRequest::decode(out.as_slice()).expect("valid protobuf");
        let res = req.resource_metrics[0].resource.as_ref().expect("resource");
        assert_eq!(attr(&res.attributes, "deployment.environment"), "prod");
        assert_eq!(attr(&res.attributes, "service.name"), "node");
        let names = res
            .attributes
            .iter()
            .filter(|kv| kv.key == "service.name")
            .count();
        assert_eq!(names, 1, "service.name appears once");
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

    fn metric_names(req: &ExportMetricsServiceRequest) -> Vec<String> {
        req.resource_metrics
            .iter()
            .flat_map(|rm| &rm.scope_metrics)
            .flat_map(|sm| &sm.metrics)
            .map(|m| m.name.clone())
            .collect()
    }

    #[test]
    fn types_are_matched_case_insensitively() {
        let b = batch(&[("reqs_total", 1.0, None, Some("COUNTER"), &[])]);
        let req = decode(&b);
        let m = &req.resource_metrics[0].scope_metrics[0].metrics[0];
        assert!(matches!(m.data, Some(Data::Sum(_))));
    }

    #[test]
    fn le_and_quantile_stay_attributes_outside_their_kind() {
        let b = batch(&[
            ("g", 1.0, None, Some("gauge"), &[("le", "1")]),
            (
                "c_total",
                2.0,
                None,
                Some("counter"),
                &[("quantile", "0.5")],
            ),
            (
                "h_count",
                1.0,
                None,
                Some("histogram"),
                &[("quantile", "x")],
            ),
            (
                "h_bucket",
                1.0,
                None,
                Some("histogram"),
                &[("le", "+Inf"), ("quantile", "x")],
            ),
        ]);
        let req = decode(&b);
        let metrics = &req.resource_metrics[0].scope_metrics[0].metrics;
        let Some(Data::Gauge(g)) = &metrics[0].data else {
            panic!("expected gauge")
        };
        assert_eq!(attr(&g.data_points[0].attributes, "le"), "1");
        let Some(Data::Sum(c)) = &metrics[1].data else {
            panic!("expected sum")
        };
        assert_eq!(attr(&c.data_points[0].attributes, "quantile"), "0.5");
        let Some(Data::Histogram(h)) = &metrics[2].data else {
            panic!("expected histogram")
        };
        // `quantile` is an ordinary label on a histogram.
        assert_eq!(h.data_points.len(), 1);
        assert_eq!(attr(&h.data_points[0].attributes, "quantile"), "x");
        assert!(h.data_points[0].attributes.iter().all(|kv| kv.key != "le"));
    }

    #[test]
    fn samples_at_different_timestamps_are_separate_points() {
        let h = Some("histogram");
        let b = batch(&[
            ("lat_bucket", 1.0, Some(1), h, &[("le", "+Inf")]),
            ("lat_count", 1.0, Some(1), h, &[]),
            ("lat_bucket", 3.0, Some(2), h, &[("le", "+Inf")]),
            ("lat_count", 3.0, Some(2), h, &[]),
        ]);
        let req = decode(&b);
        let Some(Data::Histogram(hist)) = &req.resource_metrics[0].scope_metrics[0].metrics[0].data
        else {
            panic!("expected histogram")
        };
        let got: Vec<(u64, u64)> = hist
            .data_points
            .iter()
            .map(|p| (p.time_unix_nano, p.count))
            .collect();
        assert_eq!(got, vec![(1_000_000, 1), (2_000_000, 3)]);
    }

    #[test]
    fn incomplete_or_inconsistent_composites_are_dropped() {
        let h = Some("histogram");
        let s = Some("summary");
        let b = batch(&[
            // No count and no +Inf bucket.
            ("a_bucket", 1.0, None, h, &[("le", "1")]),
            // +Inf bucket disagrees with _count.
            ("b_bucket", 2.0, None, h, &[("le", "+Inf")]),
            ("b_count", 3.0, None, h, &[]),
            // Decreasing cumulative counts.
            ("c_bucket", 5.0, None, h, &[("le", "1")]),
            ("c_bucket", 4.0, None, h, &[("le", "+Inf")]),
            ("c_count", 4.0, None, h, &[]),
            // Summary without _count.
            ("d", 0.1, None, s, &[("quantile", "0.5")]),
            // NaN _count and no +Inf bucket.
            ("e_bucket", 3.0, None, h, &[("le", "1")]),
            ("e_count", f64::NAN, None, h, &[]),
            // Negative summary _count.
            ("f", 0.1, None, s, &[("quantile", "0.5")]),
            ("f_count", -1.0, None, s, &[]),
            // Complete histogram: count taken from the +Inf bucket.
            ("ok_bucket", 2.0, None, h, &[("le", "+Inf")]),
        ]);
        let req = decode(&b);
        assert_eq!(metric_names(&req), vec!["ok".to_string()]);
    }

    #[test]
    fn summary_family_named_like_a_suffix_keeps_quantiles() {
        let s = Some("summary");
        let b = batch(&[
            ("rpc_sum", 0.2, None, s, &[("quantile", "0.5")]),
            ("rpc_sum_sum", 4.0, None, s, &[]),
            ("rpc_sum_count", 8.0, None, s, &[]),
        ]);
        let req = decode(&b);
        let metrics = &req.resource_metrics[0].scope_metrics[0].metrics;
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "rpc_sum");
        let Some(Data::Summary(sum)) = &metrics[0].data else {
            panic!("expected summary")
        };
        let p = &sum.data_points[0];
        assert_eq!((p.count, p.sum, p.quantile_values.len()), (8, 4.0, 1));
    }

    #[test]
    fn start_time_samples_are_not_exported() {
        let b = batch(&[
            ("jobs_total", 3.0, None, Some("counter"), &[]),
            ("jobs_created", 1.7e9, None, Some("created"), &[]),
        ]);
        let req = decode(&b);
        assert_eq!(metric_names(&req), vec!["jobs_total".to_string()]);
    }

    #[test]
    fn created_series_next_to_a_total_is_kept() {
        // Only `__type__ = created` marks a start time; any other
        // `*_created` series is real, typed or not.
        let c = Some("counter");
        let b = batch(&[
            ("jobs_total", 10.0, None, c, &[]),
            ("jobs_created", 5.0, None, c, &[]),
            ("foo_total", 3.0, None, c, &[]),
            ("foo_created", 4.0, None, None, &[]),
        ]);
        let req = decode(&b);
        assert_eq!(
            metric_names(&req),
            vec![
                "jobs_total".to_string(),
                "jobs_created".to_string(),
                "foo_total".to_string(),
                "foo_created".to_string()
            ]
        );
    }

    #[test]
    fn only_scanner_added_label_prefixes_are_stripped() {
        let b = batch(&[(
            "up",
            1.0,
            None,
            None,
            &[("label:__type__", "a"), ("label:zone", "b")],
        )]);
        let req = decode(&b);
        let Some(Data::Gauge(g)) = &req.resource_metrics[0].scope_metrics[0].metrics[0].data else {
            panic!("expected gauge")
        };
        let attrs = &g.data_points[0].attributes;
        assert_eq!(attr(attrs, "__type__"), "a");
        assert_eq!(attr(attrs, "label:zone"), "b");
    }

    #[test]
    fn non_numeric_value_column_is_rejected() {
        let b = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new(NAME_COL, DataType::Utf8, true),
                Field::new(VALUE_COL, DataType::Utf8, true),
            ])),
            vec![
                Arc::new(StringArray::from(vec!["up"])),
                Arc::new(StringArray::from(vec!["1"])),
            ],
        )
        .expect("batch");
        let mut out = Vec::new();
        let err = encode_metrics_batch(&b, &[], 0, &mut out).expect_err("string value");
        assert!(err.contains("numeric __value__"), "{err}");
    }

    #[test]
    fn struct_columns_are_skipped() {
        use arrow::array::StructArray;
        let inner: ArrayRef = Arc::new(Int64Array::from(vec![1]));
        let st = StructArray::from(vec![(
            Arc::new(Field::new("x", DataType::Int64, true)),
            inner,
        )]);
        let b = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new(NAME_COL, DataType::Utf8, true),
                Field::new(VALUE_COL, DataType::Float64, true),
                Field::new("nested", st.data_type().clone(), true),
            ])),
            vec![
                Arc::new(StringArray::from(vec!["up"])),
                Arc::new(Float64Array::from(vec![1.0])),
                Arc::new(st),
            ],
        )
        .expect("batch");
        let req = decode(&b);
        let Some(Data::Gauge(g)) = &req.resource_metrics[0].scope_metrics[0].metrics[0].data else {
            panic!("expected gauge")
        };
        assert!(g.data_points[0].attributes.is_empty());
    }

    #[test]
    fn negative_timestamp_falls_back_to_default() {
        let b = batch(&[("up", 1.0, Some(-5), None, &[])]);
        let req = decode(&b);
        let Some(Data::Gauge(g)) = &req.resource_metrics[0].scope_metrics[0].metrics[0].data else {
            panic!("expected gauge")
        };
        assert_eq!(g.data_points[0].time_unix_nano, 99);
    }

    #[test]
    fn histogram_bucket_counts_add_up_to_the_total() {
        let h = Some("histogram");
        let b = batch(&[
            ("lat_bucket", 0.6, None, h, &[("le", "1")]),
            ("lat_bucket", 1.2, None, h, &[("le", "2")]),
            ("lat_count", 1.2, None, h, &[]),
        ]);
        let req = decode(&b);
        let Some(Data::Histogram(hist)) = &req.resource_metrics[0].scope_metrics[0].metrics[0].data
        else {
            panic!("expected histogram")
        };
        let p = &hist.data_points[0];
        assert_eq!(p.bucket_counts.iter().sum::<u64>(), p.count);
        assert_eq!(p.count, 1);
    }

    #[test]
    fn empty_and_absent_labels_are_the_same_series() {
        let h = Some("histogram");
        let b = batch(&[
            ("h_bucket", 1.0, None, h, &[("a", ""), ("le", "+Inf")]),
            ("h_sum", 2.0, None, h, &[]),
            ("h_count", 1.0, None, h, &[]),
        ]);
        let req = decode(&b);
        let Some(Data::Histogram(hist)) = &req.resource_metrics[0].scope_metrics[0].metrics[0].data
        else {
            panic!("expected histogram")
        };
        assert_eq!(hist.data_points.len(), 1);
        let p = &hist.data_points[0];
        assert_eq!((p.count, p.sum), (1, Some(2.0)));
        assert!(p.attributes.is_empty(), "empty label is not exported");
    }

    #[test]
    fn rows_encoding_to_the_same_time_group_together() {
        // With a default time of 0, no timestamp and an explicit 0 ms both
        // encode as 0 ns, so they are one point.
        let s = Some("summary");
        let b = batch(&[
            ("rpc", 0.1, None, s, &[("quantile", "0.5")]),
            ("rpc_count", 4.0, Some(0), s, &[]),
        ]);
        let mut out = Vec::new();
        encode_metrics_batch(&b, &[], 0, &mut out).expect("encode");
        let req = ExportMetricsServiceRequest::decode(out.as_slice()).expect("valid protobuf");
        let Some(Data::Summary(sum)) = &req.resource_metrics[0].scope_metrics[0].metrics[0].data
        else {
            panic!("expected summary")
        };
        assert_eq!(sum.data_points.len(), 1);
        assert_eq!(sum.data_points[0].quantile_values.len(), 1);
    }

    #[test]
    fn missing_and_unusable_timestamps_group_together() {
        let s = Some("summary");
        let b = batch(&[
            ("rpc", 0.1, None, s, &[("quantile", "0.5")]),
            ("rpc_count", 4.0, Some(-1), s, &[]),
        ]);
        let req = decode(&b);
        let Some(Data::Summary(sum)) = &req.resource_metrics[0].scope_metrics[0].metrics[0].data
        else {
            panic!("expected summary")
        };
        assert_eq!(sum.data_points.len(), 1);
        let p = &sum.data_points[0];
        assert_eq!((p.count, p.quantile_values.len()), (4, 1));
        assert_eq!(p.time_unix_nano, 99);
    }
}
