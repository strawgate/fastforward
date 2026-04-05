//! Fuzz the CRI parser and reassembler with arbitrary bytes.
//!
//! The CRI parser is the first code to touch raw log data in Kubernetes
//! deployments. This target exercises:
//! - `parse_cri_line` — single-line parsing of untrusted container runtime output.
//! - `CriReassembler` — partial line reassembly (P/F flag handling).
//! - `process_cri_to_buf` — full chunk processing pipeline including JSON
//!   prefix injection.
//!
//! Verifies that no combination of input bytes causes a panic.

#![no_main]
use libfuzzer_sys::fuzz_target;
use logfwd_core::cri::{parse_cri_line, process_cri_to_buf, CriReassembler};

fuzz_target!(|data: &[u8]| {
    // --- Single-line parsing ---
    if let Some(cri) = parse_cri_line(data) {
        // Exercise field access on successfully parsed lines.
        let _ts = cri.timestamp;
        let _stream = cri.stream;
        let _full = cri.is_full;
        let _msg = cri.message;
    }

    // --- Chunk processing without JSON prefix ---
    let mut reassembler = CriReassembler::new(1024 * 1024);
    let mut out = Vec::new();
    let (_count, _errors) = process_cri_to_buf(data, &mut reassembler, None, &mut out);

    // --- Chunk processing with a JSON prefix ---
    let mut reassembler2 = CriReassembler::new(1024 * 1024);
    let mut out2 = Vec::new();
    let prefix = b"\"k8s.pod\":\"fuzz\",";
    let (_count2, _errors2) =
        process_cri_to_buf(data, &mut reassembler2, Some(prefix), &mut out2);

    // --- Full CRI → Scanner pipeline with validate_utf8: true ---
    // When input is valid UTF-8, exercises the validation code path through
    // the CRI parser and into the scanner. When invalid, the scanner returns
    // Err early — either way, no panics.
    if !out.is_empty() {
        use logfwd_arrow::scanner::Scanner;
        use logfwd_core::scan_config::ScanConfig;

        // Without UTF-8 validation (baseline)
        let config = ScanConfig {
            wanted_fields: vec![],
            extract_all: true,
            keep_raw: false,
            validate_utf8: false,
        };
        let mut scanner = Scanner::new(config);
        let _ = scanner.scan_detached(bytes::Bytes::from(out.clone()));

        // With UTF-8 validation
        let config_v = ScanConfig {
            wanted_fields: vec![],
            extract_all: true,
            keep_raw: false,
            validate_utf8: true,
        };
        let mut scanner_v = Scanner::new(config_v);
        let _ = scanner_v.scan_detached(bytes::Bytes::from(out));
    }
});
