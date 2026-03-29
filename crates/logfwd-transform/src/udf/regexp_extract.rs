//! UDF: regexp_extract(string, pattern, group_index) -> Utf8
//!
//! Spark-compatible regex extraction. Returns the capture group at the given
//! index (1-based), or the full match if index is 0. Returns NULL on no match.
//!
//! ```sql
//! SELECT regexp_extract(message_str, 'status=(\d+)', 1) AS status FROM logs
//! SELECT regexp_extract(message_str, 'duration=(\d+)ms', 1) AS duration FROM logs
//! ```

use std::any::Any;
use std::sync::{Arc, Mutex};

use arrow::array::{Array, AsArray, StringBuilder};
use arrow::datatypes::DataType;

use datafusion::common::Result as DfResult;
use datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDFImpl, Signature, TypeSignature, Volatility,
};

use regex::Regex;

/// UDF: regexp_extract(string, pattern, group_index) -> Utf8
///
/// - `string`: the input column (Utf8)
/// - `pattern`: regex pattern with capture groups (Utf8 literal)
/// - `group_index`: 0 for full match, 1+ for capture groups (Int64)
///
/// Returns NULL when the pattern doesn't match or the group index is out of range.
///
/// The compiled `Regex` is cached in the struct after the first invocation so
/// repeated calls with the same pattern pay the compilation cost only once.
#[derive(Debug)]
pub struct RegexpExtractUdf {
    signature: Signature,
    /// Cached compiled regex: stores `(pattern_string, compiled_regex)`.
    /// Recompiled only when the pattern string changes.
    cached_regex: Mutex<Option<(String, Regex)>>,
}

impl Default for RegexpExtractUdf {
    fn default() -> Self {
        Self::new()
    }
}

impl RegexpExtractUdf {
    pub fn new() -> Self {
        Self {
            signature: Signature::new(
                TypeSignature::Exact(vec![DataType::Utf8, DataType::Utf8, DataType::Int64]),
                Volatility::Immutable,
            ),
            cached_regex: Mutex::new(None),
        }
    }
}

impl ScalarUDFImpl for RegexpExtractUdf {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "regexp_extract"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> DfResult<DataType> {
        Ok(DataType::Utf8)
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> DfResult<ColumnarValue> {
        let input = &args.args[0];
        let pattern = &args.args[1];
        let group_idx = &args.args[2];

        // Extract the pattern string (must be a constant/scalar).
        let pattern_str = match pattern {
            ColumnarValue::Scalar(datafusion::common::ScalarValue::Utf8(Some(s))) => s.clone(),
            ColumnarValue::Scalar(s) => {
                let s = s.to_string();
                s.trim_matches('"').trim_matches('\'').to_string()
            }
            ColumnarValue::Array(arr) => {
                let str_arr = arr.as_string::<i32>();
                if str_arr.len() == 0 || str_arr.is_null(0) {
                    return Ok(ColumnarValue::Scalar(
                        datafusion::common::ScalarValue::Utf8(None),
                    ));
                }
                str_arr.value(0).to_string()
            }
        };

        // Retrieve from cache or compile once and cache for subsequent calls.
        let re = {
            let mut cache = self.cached_regex.lock().unwrap();
            match &*cache {
                Some((pat, re)) if pat == &pattern_str => re.clone(),
                _ => {
                    let re = Regex::new(&pattern_str).map_err(|e| {
                        datafusion::error::DataFusionError::Execution(format!(
                            "regexp_extract: invalid pattern '{}': {}",
                            pattern_str, e
                        ))
                    })?;
                    *cache = Some((pattern_str.clone(), re.clone()));
                    re
                }
            }
        };

        // Extract group index.
        let idx = match group_idx {
            ColumnarValue::Scalar(s) => {
                if let datafusion::common::ScalarValue::Int64(Some(v)) = s {
                    *v as usize
                } else {
                    0
                }
            }
            ColumnarValue::Array(arr) => {
                let int_arr = arr.as_primitive::<arrow::datatypes::Int64Type>();
                if int_arr.is_empty() || int_arr.is_null(0) {
                    0
                } else {
                    int_arr.value(0) as usize
                }
            }
        };

        match input {
            ColumnarValue::Array(array) => {
                let str_array = array.as_string::<i32>();
                let mut builder =
                    StringBuilder::with_capacity(str_array.len(), str_array.len() * 32);

                for i in 0..str_array.len() {
                    if str_array.is_null(i) {
                        builder.append_null();
                        continue;
                    }
                    let val = str_array.value(i);
                    match re.captures(val) {
                        Some(caps) => match caps.get(idx) {
                            Some(m) => builder.append_value(m.as_str()),
                            None => builder.append_null(),
                        },
                        None => builder.append_null(),
                    }
                }

                Ok(ColumnarValue::Array(Arc::new(builder.finish())))
            }
            ColumnarValue::Scalar(scalar) => {
                let val = scalar.to_string();
                let val = val.trim_matches('"').trim_matches('\'');
                match re.captures(val) {
                    Some(caps) => match caps.get(idx) {
                        Some(m) => Ok(ColumnarValue::Scalar(
                            datafusion::common::ScalarValue::Utf8(Some(m.as_str().to_string())),
                        )),
                        None => Ok(ColumnarValue::Scalar(
                            datafusion::common::ScalarValue::Utf8(None),
                        )),
                    },
                    None => Ok(ColumnarValue::Scalar(
                        datafusion::common::ScalarValue::Utf8(None),
                    )),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Array, Int64Array, StringArray};
    use arrow::datatypes::{Field, Schema};
    use arrow::record_batch::RecordBatch;
    use datafusion::logical_expr::ScalarUDF;
    use datafusion::prelude::*;

    /// Helper to run a SQL query with regexp_extract registered.
    async fn run_sql(batch: RecordBatch, sql: &str) -> RecordBatch {
        let ctx = SessionContext::new();
        ctx.register_udf(ScalarUDF::from(RegexpExtractUdf::new()));
        ctx.register_udf(ScalarUDF::from(crate::IntCastUdf::new()));
        let table =
            datafusion::datasource::MemTable::try_new(batch.schema(), vec![vec![batch]]).unwrap();
        ctx.register_table("logs", Arc::new(table)).unwrap();
        let df = ctx.sql(sql).await.unwrap();
        let batches = df.collect().await.unwrap();
        batches.into_iter().next().unwrap()
    }

    fn make_batch() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![Field::new("msg", DataType::Utf8, true)]));
        let msgs: Arc<dyn Array> = Arc::new(StringArray::from(vec![
            Some("GET /api/users 200 15ms"),
            Some("POST /api/orders 500 230ms"),
            Some("no match here"),
            None,
        ]));
        RecordBatch::try_new(schema, vec![msgs]).unwrap()
    }

    #[test]
    fn test_extract_group_1() {
        let batch = make_batch();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(run_sql(
            batch,
            "SELECT regexp_extract(msg, '(GET|POST) (\\S+) (\\d+) (\\d+)ms', 3) AS status FROM logs",
        ));

        let status = result
            .column_by_name("status")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(status.value(0), "200");
        assert_eq!(status.value(1), "500");
        assert!(status.is_null(2)); // no match
        assert!(status.is_null(3)); // NULL input
    }

    #[test]
    fn test_extract_group_0_full_match() {
        let batch = make_batch();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(run_sql(
            batch,
            "SELECT regexp_extract(msg, '\\d+ms', 0) AS duration FROM logs",
        ));

        let dur = result
            .column_by_name("duration")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(dur.value(0), "15ms");
        assert_eq!(dur.value(1), "230ms");
        assert!(dur.is_null(2));
    }

    #[test]
    fn test_extract_multiple_fields() {
        let batch = make_batch();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(run_sql(
            batch,
            "SELECT \
                regexp_extract(msg, '(GET|POST) (\\S+) (\\d+) (\\d+)ms', 1) AS method, \
                regexp_extract(msg, '(GET|POST) (\\S+) (\\d+) (\\d+)ms', 2) AS path, \
                int(regexp_extract(msg, '(GET|POST) (\\S+) (\\d+) (\\d+)ms', 4)) AS duration_ms \
             FROM logs",
        ));

        let method = result
            .column_by_name("method")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(method.value(0), "GET");
        assert_eq!(method.value(1), "POST");

        let path = result
            .column_by_name("path")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(path.value(0), "/api/users");
        assert_eq!(path.value(1), "/api/orders");

        // int() composed with regexp_extract
        let dur = result
            .column_by_name("duration_ms")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(dur.value(0), 15);
        assert_eq!(dur.value(1), 230);
    }

    /// Verify that the compiled regex is cached: the same `Regex` pointer should
    /// be reused across two `invoke_with_args` calls on the same UDF instance.
    #[test]
    fn test_regex_is_cached_across_invocations() {
        use arrow::array::StringArray;
        use datafusion::common::ScalarValue;
        use datafusion::logical_expr::{ColumnarValue, ScalarFunctionArgs};

        let udf = RegexpExtractUdf::new();
        let pattern = ColumnarValue::Scalar(ScalarValue::Utf8(Some(r"\d+".to_string())));
        let group = ColumnarValue::Scalar(ScalarValue::Int64(Some(0)));

        let make_args = |pattern: ColumnarValue, group: ColumnarValue| ScalarFunctionArgs {
            args: vec![
                ColumnarValue::Array(Arc::new(StringArray::from(vec![Some("abc 42 def")])) as _),
                pattern,
                group,
            ],
            number_rows: 1,
            return_type: &DataType::Utf8,
        };

        // First invocation — populates the cache.
        let r1 = udf
            .invoke_with_args(make_args(pattern.clone(), group.clone()))
            .unwrap();

        // Second invocation with the same pattern — must hit the cache and return
        // the same result.
        let r2 = udf
            .invoke_with_args(make_args(pattern.clone(), group.clone()))
            .unwrap();

        // Both calls should extract the same value.
        let to_str = |cv: ColumnarValue| match cv {
            ColumnarValue::Array(a) => a
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(0)
                .to_string(),
            _ => panic!("expected array"),
        };
        assert_eq!(to_str(r1), "42");
        assert_eq!(to_str(r2), "42");

        // Cache should still hold the last pattern.
        let cached = udf.cached_regex.lock().unwrap();
        assert!(cached.is_some());
        assert_eq!(cached.as_ref().unwrap().0, r"\d+");
    }
}
