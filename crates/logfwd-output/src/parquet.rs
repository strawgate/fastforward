use std::io;

use arrow::record_batch::RecordBatch;

use super::{BatchMetadata, OutputSink};

// ---------------------------------------------------------------------------
// ParquetSink (placeholder)
// ---------------------------------------------------------------------------

/// Writes log records to Parquet files.
pub(crate) struct ParquetSink {
    name: String,
    path: String,
}

impl ParquetSink {
    pub(crate) fn new(name: String, path: String) -> Self {
        ParquetSink { name, path }
    }
}

impl OutputSink for ParquetSink {
    fn send_batch(&mut self, _batch: &RecordBatch, _metadata: &BatchMetadata) -> io::Result<()> {
        // TODO: implement Parquet writing
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}
