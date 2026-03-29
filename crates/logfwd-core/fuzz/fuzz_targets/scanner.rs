//! Fuzz the JSON scanner with arbitrary bytes.
//!
//! Exercises both the scalar Scanner and the SIMD SimdScanner
//! (with IndexedBatchBuilder) plus the ColumnarSimdScanner.

#![no_main]
use libfuzzer_sys::fuzz_target;
use logfwd_core::scanner::{ScanConfig, Scanner};
use logfwd_core::simd_scanner::{ColumnarSimdScanner, SimdScanner};

fn validate_batch(batch: &arrow::record_batch::RecordBatch, label: &str) {
    let num_rows = batch.num_rows();
    let schema = batch.schema();
    for col_idx in 0..batch.num_columns() {
        assert_eq!(
            batch.column(col_idx).len(),
            num_rows,
            "{label}: column '{}' length {} != num_rows {num_rows}",
            schema.field(col_idx).name(),
            batch.column(col_idx).len(),
        );
    }
}

fn make_extract_all_config() -> ScanConfig {
    ScanConfig {
        wanted_fields: vec![],
        extract_all: true,
        keep_raw: true,
    }
}

fn make_pushdown_config() -> ScanConfig {
    ScanConfig {
        wanted_fields: vec![
            logfwd_core::scanner::FieldSpec {
                name: "level".to_string(),
                aliases: vec![],
            },
            logfwd_core::scanner::FieldSpec {
                name: "msg".to_string(),
                aliases: vec![],
            },
        ],
        extract_all: false,
        keep_raw: false,
    }
}

fuzz_target!(|data: &[u8]| {
    // The builders use from_utf8_unchecked on values extracted from the input.
    // Non-UTF-8 input would cause UB, so reject it early.
    if std::str::from_utf8(data).is_err() {
        return;
    }

    // --- Scalar Scanner ---
    {
        let mut scanner = Scanner::new(make_extract_all_config(), 128);
        let batch = scanner.scan(data);
        validate_batch(&batch, "scalar_extract_all");
    }
    {
        let mut scanner = Scanner::new(make_pushdown_config(), 128);
        let batch = scanner.scan(data);
        validate_batch(&batch, "scalar_pushdown");
    }

    // --- SIMD Scanner (IndexedBatchBuilder) ---
    {
        let mut scanner = SimdScanner::new(make_extract_all_config(), 128);
        let batch = scanner.scan(data);
        validate_batch(&batch, "simd_extract_all");
    }
    {
        let mut scanner = SimdScanner::new(make_pushdown_config(), 128);
        let batch = scanner.scan(data);
        validate_batch(&batch, "simd_pushdown");
    }

    // --- Columnar SIMD Scanner ---
    {
        let mut scanner = ColumnarSimdScanner::new(make_extract_all_config(), 128);
        let batch = scanner.scan(data);
        validate_batch(&batch, "columnar_extract_all");
    }
    {
        let mut scanner = ColumnarSimdScanner::new(make_pushdown_config(), 128);
        let batch = scanner.scan(data);
        validate_batch(&batch, "columnar_pushdown");
    }
});
