//! Pre-processing pass that optimizes multiple json() UDF calls.
//!
//! When the user's SQL contains multiple `json(_raw, 'f0'), json_int(_raw, 'f1')`
//! calls sharing the same `_raw` source, this module:
//!
//! 1. Extracts all referenced field names from the SQL
//! 2. Runs our SIMD scanner once to extract those fields from `_raw`
//! 3. Adds the extracted columns to the RecordBatch
//! 4. Rewrites the SQL to use direct column references instead of UDF calls
//!
//! This turns N parses into 1 parse, closing the performance gap between
//! the raw-first UDF approach and the pre-scan approach.

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;

use logfwd_arrow::StreamingSimdScanner;
use logfwd_core::scan_config::{FieldSpec, ScanConfig};

/// A json field extraction request parsed from the SQL.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JsonFieldRef {
    /// The field name in JSON (e.g., "status")
    pub field: String,
    /// The UDF used: "json", "json_int", or "json_float"
    pub udf_name: String,
    /// The alias to use in the rewritten SQL (e.g., "status" or user's AS alias)
    pub output_name: String,
}

/// Scan the SQL for json()/json_int()/json_float() calls and extract field references.
///
/// Returns the list of field references found, or empty if no json UDFs are used.
pub fn extract_json_refs(sql: &str) -> Vec<JsonFieldRef> {
    let mut refs = Vec::new();

    // Simple regex-free parser: find json(, json_int(, json_float( followed by
    // _raw and a string literal.
    for udf in &["json_float", "json_int", "json"] {
        let pattern = format!("{udf}(");
        let mut search_from = 0;
        while let Some(pos) = sql[search_from..].find(&pattern) {
            let abs_pos = search_from + pos;
            let after_paren = abs_pos + pattern.len();

            // Find the closing paren
            if let Some(close) = sql[after_paren..].find(')') {
                let args_str = &sql[after_paren..after_paren + close];
                // Parse args: should be "_raw, 'field_name'" or "_raw, 'field_name'"
                let parts: Vec<&str> = args_str.splitn(2, ',').collect();
                if parts.len() == 2 {
                    let col = parts[0].trim();
                    let key_raw = parts[1].trim();
                    // Strip quotes from key
                    if col == "_raw"
                        && key_raw.len() >= 2
                        && (key_raw.starts_with('\'') || key_raw.starts_with('"'))
                    {
                        let key = &key_raw[1..key_raw.len() - 1];
                        // Generate output column name based on UDF type
                        let output_name = match *udf {
                            "json_int" => format!("{key}__int"),
                            "json_float" => format!("{key}__float"),
                            _ => format!("{key}__str"),
                        };
                        refs.push(JsonFieldRef {
                            field: key.to_string(),
                            udf_name: udf.to_string(),
                            output_name,
                        });
                    }
                }
            }
            search_from = abs_pos + pattern.len();
        }
    }

    refs
}

/// Run the SIMD scanner on the `_raw` column for the requested fields,
/// then add the extracted columns to the batch.
///
/// Returns the enriched batch and a mapping from original UDF call pattern
/// to the new column name.
pub fn preprocess_json_batch(
    batch: &RecordBatch,
    refs: &[JsonFieldRef],
) -> Result<(RecordBatch, HashMap<String, String>), String> {
    if refs.is_empty() {
        return Ok((batch.clone(), HashMap::new()));
    }

    let raw_col = batch
        .column_by_name("_raw")
        .ok_or("batch has no _raw column")?;
    let raw_array = raw_col
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .ok_or("_raw column is not Utf8")?;

    // Collect unique field names
    let unique_fields: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        refs.iter()
            .filter(|r| seen.insert(r.field.clone()))
            .map(|r| r.field.clone())
            .collect()
    };

    // Reconstruct NDJSON buffer
    let mut buf = Vec::with_capacity(raw_array.len() * 128);
    for i in 0..raw_array.len() {
        if !raw_array.is_null(i) {
            buf.extend_from_slice(raw_array.value(i).as_bytes());
        }
        buf.push(b'\n');
    }

    // Run scanner for the requested fields
    let config = ScanConfig {
        wanted_fields: unique_fields
            .iter()
            .map(|f| FieldSpec {
                name: f.clone(),
                aliases: vec![],
            })
            .collect(),
        extract_all: false,
        keep_raw: false,
        validate_utf8: false,
    };

    let mut scanner = StreamingSimdScanner::new(config);
    let parsed = scanner
        .scan(bytes::Bytes::from(buf))
        .map_err(|e| format!("scanner error: {e}"))?;

    // Build new columns and the rewrite mapping
    let mut extra_fields: Vec<Arc<Field>> = Vec::new();
    let mut extra_columns: Vec<ArrayRef> = Vec::new();
    let mut rewrite_map: HashMap<String, String> = HashMap::new();

    let mut seen_outputs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in refs {
        if seen_outputs.contains(&r.output_name) {
            // Already added this column — just add the rewrite mapping
            for q in ['\'', '"'] {
                let pattern = format!("{}(_raw, {q}{}{q})", r.udf_name, r.field);
                rewrite_map.insert(pattern, r.output_name.clone());
            }
            continue;
        }
        seen_outputs.insert(r.output_name.clone());
        // Find the scanner output column for this field + type
        let col = match r.udf_name.as_str() {
            "json_int" => {
                // Prefer _int suffix
                parsed
                    .column_by_name(&format!("{}_int", r.field))
                    .or_else(|| parsed.column_by_name(&r.field))
            }
            "json_float" => parsed
                .column_by_name(&format!("{}_float", r.field))
                .or_else(|| parsed.column_by_name(&r.field)),
            _ => {
                // json() — prefer _str, cast to Utf8
                parsed
                    .column_by_name(&format!("{}_str", r.field))
                    .or_else(|| parsed.column_by_name(&format!("{}_int", r.field)))
                    .or_else(|| parsed.column_by_name(&format!("{}_float", r.field)))
                    .or_else(|| parsed.column_by_name(&r.field))
            }
        };

        let col = match col {
            Some(c) => Arc::clone(c),
            None => {
                // Field not found — create null column of the right type
                let dt = match r.udf_name.as_str() {
                    "json_int" => DataType::Int64,
                    "json_float" => DataType::Float64,
                    _ => DataType::Utf8,
                };
                arrow::array::new_null_array(&dt, batch.num_rows())
            }
        };

        // For json(), cast to Utf8 if not already
        let col = if r.udf_name == "json" && *col.data_type() != DataType::Utf8 {
            arrow::compute::cast(&col, &DataType::Utf8).map_err(|e| format!("cast error: {e}"))?
        } else {
            col
        };

        let output_name = &r.output_name;
        extra_fields.push(Arc::new(Field::new(
            output_name,
            col.data_type().clone(),
            true,
        )));
        extra_columns.push(col);

        // Build the rewrite pattern: "json(_raw, 'field')" -> "output_name"
        // Account for both single and double quotes
        for q in ['\'', '"'] {
            let pattern = format!("{}(_raw, {q}{}{q})", r.udf_name, r.field);
            rewrite_map.insert(pattern, output_name.clone());
        }
    }

    // Build enriched batch: original columns + extracted columns
    let mut all_fields: Vec<Arc<Field>> = batch.schema().fields().iter().cloned().collect();
    all_fields.extend(extra_fields);

    let mut all_columns: Vec<ArrayRef> = batch.columns().to_vec();
    all_columns.extend(extra_columns);

    let new_schema = Arc::new(Schema::new(all_fields));
    let enriched =
        RecordBatch::try_new(new_schema, all_columns).map_err(|e| format!("batch error: {e}"))?;

    Ok((enriched, rewrite_map))
}

/// Rewrite the SQL, replacing json UDF calls with column references.
pub fn rewrite_sql(
    sql: &str,
    rewrite_map: &HashMap<String, String, impl std::hash::BuildHasher>,
) -> String {
    let mut result = sql.to_string();
    // Sort by longest pattern first to avoid partial replacements
    let mut patterns: Vec<(&String, &String)> = rewrite_map.iter().collect();
    patterns.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    for (pattern, replacement) in patterns {
        result = result.replace(pattern.as_str(), replacement.as_str());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::StringArray;

    fn make_raw_batch(lines: Vec<&str>) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![Field::new("_raw", DataType::Utf8, false)]));
        RecordBatch::try_new(schema, vec![Arc::new(StringArray::from(lines)) as ArrayRef]).unwrap()
    }

    #[test]
    fn test_extract_json_refs() {
        let sql = "SELECT json(_raw, 'status') as s, json_int(_raw, 'code') as c FROM logs WHERE json_float(_raw, 'dur') > 1.0";
        let refs = extract_json_refs(sql);
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].field, "dur");
        assert_eq!(refs[0].udf_name, "json_float");
        assert_eq!(refs[1].field, "code");
        assert_eq!(refs[1].udf_name, "json_int");
        // json() matches last because we search json_float and json_int first
        assert_eq!(refs[2].field, "status");
        assert_eq!(refs[2].udf_name, "json");
    }

    #[test]
    fn test_extract_no_refs() {
        let sql = "SELECT * FROM logs WHERE level = 'ERROR'";
        let refs = extract_json_refs(sql);
        assert!(refs.is_empty());
    }

    #[test]
    fn test_preprocess_adds_columns() {
        let batch = make_raw_batch(vec![
            r#"{"status": 200, "level": "INFO"}"#,
            r#"{"status": 500, "level": "ERROR"}"#,
        ]);
        let refs = vec![
            JsonFieldRef {
                field: "status".into(),
                udf_name: "json_int".into(),
                output_name: "status__int".into(),
            },
            JsonFieldRef {
                field: "level".into(),
                udf_name: "json".into(),
                output_name: "level__str".into(),
            },
        ];
        let (enriched, map) = preprocess_json_batch(&batch, &refs).unwrap();

        // Should have _raw + 2 extracted columns
        assert_eq!(enriched.num_columns(), 3);
        assert!(enriched.column_by_name("status__int").is_some());
        assert!(enriched.column_by_name("level__str").is_some());

        // Check values
        let status = enriched
            .column_by_name("status__int")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap();
        assert_eq!(status.value(0), 200);
        assert_eq!(status.value(1), 500);
    }

    #[test]
    fn test_rewrite_sql() {
        let mut map = HashMap::new();
        map.insert("json(_raw, 'status')".into(), "status__str".into());
        map.insert("json_int(_raw, 'code')".into(), "code__int".into());

        let sql = "SELECT json(_raw, 'status'), json_int(_raw, 'code') FROM logs WHERE json_int(_raw, 'code') > 400";
        let rewritten = rewrite_sql(sql, &map);
        assert_eq!(
            rewritten,
            "SELECT status__str, code__int FROM logs WHERE code__int > 400"
        );
    }

    #[tokio::test]
    async fn test_full_roundtrip() {
        // End-to-end: SQL with json UDFs → preprocess → rewrite → execute
        let batch = make_raw_batch(vec![
            r#"{"status": 200, "msg": "ok"}"#,
            r#"{"status": 500, "msg": "error"}"#,
            r#"{"status": 301, "msg": "redirect"}"#,
        ]);

        let sql = "SELECT json(_raw, 'msg') as msg FROM logs WHERE json_int(_raw, 'status') > 400";

        // Step 1: extract refs
        let refs = extract_json_refs(sql);
        assert_eq!(refs.len(), 2);

        // Step 2: preprocess
        let (enriched, rewrite_map) = preprocess_json_batch(&batch, &refs).unwrap();

        // Step 3: rewrite SQL
        let new_sql = rewrite_sql(sql, &rewrite_map);

        // Step 4: execute with DataFusion
        let schema = enriched.schema();
        let ctx = datafusion::prelude::SessionContext::new();
        let table =
            datafusion::datasource::MemTable::try_new(schema, vec![vec![enriched]]).unwrap();
        ctx.register_table("logs", Arc::new(table)).unwrap();

        let df = ctx.sql(&new_sql).await.unwrap();
        let result = df.collect().await.unwrap();
        let result = result.into_iter().next().unwrap();

        assert_eq!(result.num_rows(), 1);
        let msg = result
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        assert_eq!(msg.value(0), "error");
    }
}
