//! A table provider whose data can be replaced between queries.
//!
//! [`SqlTransform`](crate::SqlTransform) plans its SQL once and reuses the
//! logical plan for every batch. A logical plan holds the table providers it
//! scans, so the providers must stay the same objects while their data
//! changes. [`SwapTable`] keeps a fixed schema and swaps the batches it serves.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::Result;
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::ExecutionPlan;

/// In-memory table with a fixed schema whose contents can be replaced.
pub(crate) struct SwapTable {
    schema: SchemaRef,
    current: Mutex<Arc<MemTable>>,
}

impl fmt::Debug for SwapTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SwapTable")
            .field("schema", &self.schema)
            .finish_non_exhaustive()
    }
}

impl SwapTable {
    /// Create a table serving `batch`. Its schema becomes the table's schema.
    pub(crate) fn try_new(batch: RecordBatch) -> Result<Self> {
        let schema = batch.schema();
        let table = MemTable::try_new(Arc::clone(&schema), vec![vec![batch]])?;
        Ok(Self {
            schema,
            current: Mutex::new(Arc::new(table)),
        })
    }

    /// Whether `batch` can replace the current data without replanning.
    pub(crate) fn accepts(&self, batch: &RecordBatch) -> bool {
        batch.schema() == self.schema
    }

    /// Serve `batch` to every scan from now on. The caller must check
    /// [`accepts`](Self::accepts) first.
    pub(crate) fn replace(&self, batch: RecordBatch) -> Result<()> {
        let table = MemTable::try_new(Arc::clone(&self.schema), vec![vec![batch]])?;
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(table);
        Ok(())
    }

    fn current(&self) -> Arc<MemTable> {
        Arc::clone(&self.current.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

#[async_trait]
impl TableProvider for SwapTable {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        self.current().scan(state, projection, filters, limit).await
    }
}
