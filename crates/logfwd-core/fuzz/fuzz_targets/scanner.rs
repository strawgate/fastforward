//! Fuzz the JSON scanner with arbitrary bytes.

#![no_main]
use libfuzzer_sys::fuzz_target;
use logfwd_core::scanner::{ScanConfig, Scanner};

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

fuzz_target!(|data: &[u8]| {
    // Extract-all mode.
    let config = ScanConfig {
        wanted_fields: vec![],
        extract_all: true,
        keep_raw: true,
    };
    let mut scanner = Scanner::new(config, 128);
    let batch = scanner.scan(data);
    validate_batch(&batch, "extract_all");

    // Field pushdown mode.
    let config2 = ScanConfig {
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
    };
    let mut scanner2 = Scanner::new(config2, 128);
    let batch2 = scanner2.scan(data);
    validate_batch(&batch2, "pushdown");
});
