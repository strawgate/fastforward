//! SQL transform execution and DataFusion context management.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use arrow::datatypes::{DataType, SchemaRef};
use arrow::record_batch::RecordBatch;

use datafusion::datasource::TableProvider;
use datafusion::logical_expr::LogicalPlan;
use datafusion::logical_expr::ScalarUDF;
use datafusion::prelude::*;

use ffwd_core::scan_config::ScanConfig;
use ffwd_types::field_names;

use crate::cast_udf::{FloatCastUdf, IntCastUdf};
use crate::swap_table::SwapTable;
use crate::{QueryAnalyzer, TransformError, enrichment, udf};
use ffwd_arrow::conflict_schema;

/// Manages a DataFusion context, compiles and caches plans, executes SQL
/// transforms against Arrow RecordBatches.
///
/// The SessionContext, UDFs, and the SQL's logical plan are created once and
/// reused across batches. The `logs` and enrichment table providers stay
/// registered and only swap the data they serve, so the cached plan stays
/// valid. This eliminates the per-batch cost of SessionContext construction,
/// UDF registration, SQL parsing, and logical planning. A schema change or an
/// enrichment table appearing or disappearing replaces a provider and
/// triggers a replan.
pub struct SqlTransform {
    user_sql: String,
    analyzer: QueryAnalyzer,
    /// Schema fingerprint for cache invalidation.
    schema_hash: u64,
    /// Enrichment tables registered alongside `logs` in each DataFusion session.
    enrichment_tables: Vec<Arc<dyn enrichment::EnrichmentTable>>,
    /// Optional geo-IP database for the `geo_lookup()` UDF.
    geo_database: Option<Arc<dyn enrichment::GeoDatabase>>,
    /// Cached DataFusion session — created once, table swapped per batch.
    pub(crate) ctx: Option<SessionContext>,
    /// Provider registered as `logs`; its data is swapped per batch.
    logs_table: Option<Arc<SwapTable>>,
    /// Providers registered for `enrichment_tables`, by index. `None` while
    /// a table has no snapshot.
    enrichment_slots: Vec<Option<Arc<SwapTable>>>,
    /// Logical plan for `user_sql`, reused while the registered providers
    /// stay the same. Cleared whenever a provider is replaced.
    pub(crate) plan: Option<LogicalPlan>,
    /// Column types the input is known to produce, used by `validate_plan`'s
    /// probe batch. Columns not listed here are probed as Utf8.
    probe_column_types: Vec<(String, DataType)>,
}

impl SqlTransform {
    /// Create a new SQL transform from a SQL string.
    pub fn new(sql: &str) -> Result<Self, TransformError> {
        let analyzer = QueryAnalyzer::new(sql)?;

        Ok(SqlTransform {
            user_sql: sql.to_string(),
            analyzer,
            schema_hash: 0,
            enrichment_tables: Vec::new(),
            geo_database: None,
            ctx: None,
            logs_table: None,
            enrichment_slots: Vec::new(),
            plan: None,
            probe_column_types: Vec::new(),
        })
    }

    /// Declare column types the input always produces (for example
    /// Float64 `__value__` from a Prometheus scrape), so `validate_plan`
    /// probes them with the right type.
    pub fn set_probe_column_types(&mut self, types: Vec<(String, DataType)>) {
        self.probe_column_types = types;
    }

    /// Set the geo-IP database for the `geo_lookup()` UDF.
    ///
    /// Invalidates the cached SessionContext so the UDF is re-registered
    /// with the new database on the next execute() call.
    pub fn set_geo_database(&mut self, db: Arc<dyn enrichment::GeoDatabase>) {
        self.geo_database = Some(db);
        self.ctx = None; // force re-creation with new geo UDF
    }

    /// Add an enrichment table that will be registered in each DataFusion
    /// session alongside the `logs` table. Returns an error if a table with
    /// the same name is already registered or if the name conflicts with "logs".
    ///
    /// Does NOT invalidate the cached SessionContext (unlike `set_geo_database`)
    /// because enrichment tables are deregistered/re-registered per batch in
    /// `execute()`. The context doesn't need to know about them at creation time.
    pub fn add_enrichment_table(
        &mut self,
        table: Arc<dyn enrichment::EnrichmentTable>,
    ) -> Result<(), TransformError> {
        let name = table.name();
        if name == "logs" {
            return Err(TransformError::Enrichment(
                "enrichment table cannot be named 'logs' (reserved)".to_string(),
            ));
        }
        if self.enrichment_tables.iter().any(|t| t.name() == name) {
            return Err(TransformError::Enrichment(format!(
                "duplicate enrichment table name: '{name}'"
            )));
        }
        self.enrichment_tables.push(table);
        Ok(())
    }

    /// Execute the SQL transform on a RecordBatch.
    ///
    /// Reuses the cached SessionContext and logical plan across batches.
    /// The `logs` provider serves this batch; enrichment providers serve the
    /// tables' current snapshots. The plan is optimized per batch, so
    /// volatile functions such as `now()` are evaluated each time.
    ///
    /// Schema changes (new fields in later batches) are handled automatically:
    /// a new schema registers a new provider and the SQL is planned again.
    pub async fn execute(&mut self, batch: RecordBatch) -> Result<RecordBatch, TransformError> {
        // Normalize first so schema fingerprinting is based on the actual
        // table shape presented to DataFusion.
        let batch = conflict_schema::normalize_conflict_columns(batch);

        // Invalidate the cached SessionContext when the schema changes.
        //
        // DataFusion caches logical plans inside the SessionContext. If the
        // batch schema changes between calls (new fields, type conflicts resolved
        // differently), the cached plan refers to a stale schema and execution
        // will fail or produce incorrect results. Forcing ctx = None causes
        // ensure_context() to build a fresh SessionContext with no stale plans.
        let new_hash = hash_schema(batch.schema());
        if new_hash != self.schema_hash {
            self.ctx = None;
        }
        self.schema_hash = new_hash;

        // Ensure the SessionContext exists (created once, reused across batches).
        // A new context has no tables registered, so the cached plan and table
        // handles from the old one are dropped with it.
        if self.ctx.is_none() {
            self.logs_table = None;
            self.enrichment_slots.clear();
            self.plan = None;
        }
        self.ensure_context();
        let ctx = self.ctx.as_ref().ok_or_else(|| {
            TransformError::Sql(
                "Failed to initialize SessionContext after ensure_context()".to_string(),
            )
        })?;

        // Point `logs` at this batch. The table provider stays the same object
        // while the schema is unchanged, so the cached plan stays valid; a new
        // schema registers a new provider and forces a replan.
        match &self.logs_table {
            Some(table) if table.accepts(&batch) => table
                .replace(batch)
                .map_err(|e| TransformError::Sql(format!("Failed to create MemTable: {e}")))?,
            _ => {
                let table =
                    Arc::new(SwapTable::try_new(batch).map_err(|e| {
                        TransformError::Sql(format!("Failed to create MemTable: {e}"))
                    })?);
                let _ = ctx.deregister_table("logs");
                ctx.register_table("logs", Arc::clone(&table) as Arc<dyn TableProvider>)
                    .map_err(|e| TransformError::Sql(format!("Failed to register table: {e}")))?;
                self.logs_table = Some(table);
                self.plan = None;
            }
        }

        // Refresh enrichment tables from their current snapshots, the same way.
        // If snapshot() returns None (table not loaded yet), deregister the
        // stale table — queries referencing it will fail with a clear error
        // rather than silently returning stale data.
        self.enrichment_slots
            .resize_with(self.enrichment_tables.len(), || None);
        for (et, slot) in self
            .enrichment_tables
            .iter()
            .zip(self.enrichment_slots.iter_mut())
        {
            let Some(snapshot) = et.snapshot() else {
                if slot.take().is_some() {
                    let _ = ctx.deregister_table(et.name());
                    self.plan = None;
                }
                tracing::warn!(
                    table = et.name(),
                    "enrichment table not yet loaded, skipping"
                );
                continue;
            };
            let enrichment_err = |e| {
                TransformError::Enrichment(format!(
                    "Failed to create enrichment table '{}': {e}",
                    et.name()
                ))
            };
            match slot {
                Some(table) if table.accepts(&snapshot) => {
                    table.replace(snapshot).map_err(enrichment_err)?;
                }
                _ => {
                    let table = Arc::new(SwapTable::try_new(snapshot).map_err(enrichment_err)?);
                    let _ = ctx.deregister_table(et.name());
                    ctx.register_table(et.name(), Arc::clone(&table) as Arc<dyn TableProvider>)
                        .map_err(|e| {
                            TransformError::Enrichment(format!(
                                "Failed to register enrichment table '{}': {e}",
                                et.name()
                            ))
                        })?;
                    *slot = Some(table);
                    self.plan = None;
                }
            }
        }

        // Plan the SQL once and reuse the logical plan while the tables keep
        // their providers. The cached plan is not yet optimized: `collect()`
        // optimizes it per batch, so functions like `now()` are evaluated
        // fresh each time.
        let df = match &self.plan {
            Some(plan) => DataFrame::new(ctx.state(), plan.clone()),
            None => {
                let df = ctx
                    .sql(&self.user_sql)
                    .await
                    .map_err(|e| TransformError::Sql(format!("SQL execution error: {e}")))?;
                self.plan = Some(df.logical_plan().clone());
                df
            }
        };

        // Capture the output schema before collecting so we can build an empty
        // batch when the WHERE clause filters out every row — without
        // re-executing `ctx.sql()` a second time.
        let output_schema: SchemaRef = Arc::clone(df.schema().inner());

        let batches = df
            .collect()
            .await
            .map_err(|e| TransformError::Sql(format!("Failed to collect results: {e}")))?;

        // Concat all result batches into one.
        let result = match batches.as_slice() {
            [] => Ok(RecordBatch::new_empty(output_schema)),
            [_] => {
                let mut batches = batches.into_iter();
                batches.next().ok_or_else(|| {
                    TransformError::Sql(
                        "DataFusion returned no batches after len check".to_string(),
                    )
                })
            }
            [first, ..] => {
                let schema = first.schema();
                concat_batches(&schema, &batches).map_err(TransformError::Arrow)
            }
        }?;
        Ok(result)
    }

    /// Lazily create the SessionContext with UDFs registered.
    ///
    /// Created lazily (not in new()) because set_geo_database() and
    /// add_enrichment_table() may be called after construction.
    fn ensure_context(&mut self) {
        if self.ctx.is_some() {
            return;
        }
        let ctx = SessionContext::new();

        // Register custom UDFs once — they persist across batches.
        ctx.register_udf(ScalarUDF::from(IntCastUdf::new()));
        ctx.register_udf(ScalarUDF::from(FloatCastUdf::new()));
        ctx.register_udf(ScalarUDF::from(udf::RegexpExtractUdf::new()));
        ctx.register_udf(ScalarUDF::from(udf::GrokUdf::new()));
        ctx.register_udf(ScalarUDF::from(udf::JsonExtractUdf::new(
            udf::JsonExtractMode::Str,
        )));
        ctx.register_udf(ScalarUDF::from(udf::JsonExtractUdf::new(
            udf::JsonExtractMode::Int,
        )));
        ctx.register_udf(ScalarUDF::from(udf::JsonExtractUdf::new(
            udf::JsonExtractMode::Float,
        )));
        ctx.register_udf(ScalarUDF::from(udf::HashUdf::new()));
        if let Some(ref db) = self.geo_database {
            ctx.register_udf(ScalarUDF::from(udf::geo_lookup::GeoLookupUdf::new(
                Arc::clone(db),
            )));
        }

        self.ctx = Some(ctx);
    }

    /// Synchronous wrapper around [`execute`](Self::execute) for callers that
    /// are not yet async. When called from within a tokio runtime, uses
    /// `block_in_place` + the current handle. Otherwise creates a temporary
    /// runtime.
    ///
    /// When the calling code is made async, switch to `execute().await` directly.
    pub fn execute_blocking(&mut self, batch: RecordBatch) -> Result<RecordBatch, TransformError> {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(self.execute(batch)))
            }
            Ok(_) | Err(_) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| {
                        TransformError::Sql(format!("Failed to create tokio runtime: {e}"))
                    })?;
                rt.block_on(self.execute(batch))
            }
        }
    }

    /// Get the ScanConfig for field pushdown.
    pub fn scan_config(&self) -> ScanConfig {
        self.analyzer.scan_config()
    }

    /// Get a reference to the query analyzer.
    pub fn analyzer(&self) -> &QueryAnalyzer {
        &self.analyzer
    }

    /// Validate the SQL plan by executing it against a dummy single-row batch.
    ///
    /// This forces DataFusion to plan the query (resolve columns, check for
    /// duplicate aliases, validate window specs, etc.) at validation time
    /// rather than waiting for the first real batch at runtime.
    pub fn validate_plan(&mut self) -> Result<(), TransformError> {
        use arrow::array::{ArrayRef, new_null_array};
        use arrow::datatypes::{Field, Schema};

        let probe_type = |name: &str| {
            self.probe_column_types
                .iter()
                .find(|(typed, _)| typed == name)
                .map_or(DataType::Utf8, |(_, ty)| ty.clone())
        };

        // Build a schema from the scanner-facing field names, not raw SQL
        // identifier spelling. DataFusion normalizes unquoted identifiers
        // during planning, and the scanner pushdown config mirrors that.
        let scan_config = self.analyzer.scan_config();
        let fields: Vec<Field> = if scan_config.extract_all || scan_config.wanted_fields.is_empty()
        {
            // SELECT * — a minimal representative schema, the input's typed
            // columns, and every column the query names elsewhere
            // (`WHERE __name__ = 'up'`).
            let mut fields = vec![
                Field::new(field_names::BODY, DataType::Utf8, true),
                Field::new("level", DataType::Utf8, true),
                Field::new("msg", DataType::Utf8, true),
            ];
            fields.extend(
                self.probe_column_types
                    .iter()
                    .map(|(name, ty)| Field::new(name.as_str(), ty.clone(), true)),
            );
            let mut referenced: Vec<&String> = self.analyzer.referenced_columns.iter().collect();
            referenced.sort();
            for name in referenced {
                if !fields.iter().any(|f| f.name() == name) {
                    fields.push(Field::new(name.as_str(), probe_type(name), true));
                }
            }
            fields
        } else {
            scan_config
                .wanted_fields
                .iter()
                .map(|field| Field::new(field.name.as_str(), probe_type(&field.name), true))
                .collect()
        };

        let schema = Arc::new(Schema::new(fields.clone()));
        let arrays: Vec<ArrayRef> = fields
            .iter()
            .map(|field| new_null_array(field.data_type(), 1))
            .collect();

        let batch = RecordBatch::try_new(schema, arrays)
            .map_err(|e| TransformError::Sql(format!("failed to build probe batch: {e}")))?;

        self.execute_blocking(batch)?;
        Ok(())
    }
}

/// Concatenate multiple RecordBatches into one.
fn concat_batches(
    schema: &SchemaRef,
    batches: &[RecordBatch],
) -> Result<RecordBatch, arrow::error::ArrowError> {
    arrow::compute::concat_batches(schema, batches)
}

/// Hash an Arrow schema for quick change detection.
fn hash_schema(schema: SchemaRef) -> u64 {
    let mut hasher = DefaultHasher::new();
    for field in schema.fields() {
        field.name().hash(&mut hasher);
        field.data_type().hash(&mut hasher);
        field.is_nullable().hash(&mut hasher);
    }
    hasher.finish()
}
