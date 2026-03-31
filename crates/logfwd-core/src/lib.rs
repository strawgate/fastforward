//! Core data processing for logfwd: scanner, builders, parsers, diagnostics, and enrichment.
//!
//! This crate contains the hot path: SIMD JSON scanning via [`scanner`],
//! CRI log parsing via [`cri`], OTLP encoding via [`otlp`], and the
//! [`ScanBuilder`] trait that both Arrow builder backends implement.
//! It is intentionally synchronous and allocation-free on the hot path.
//!
//! [`ScanBuilder`]: scanner::ScanBuilder

pub mod aggregator;
pub mod byte_search;
pub mod checkpoint;
/// SIMD chunk classification: pre-computes quote and backslash bitmasks over the input buffer.
pub mod chunk_classify;
pub mod compress;
pub mod cri;
/// Pipeline metrics and diagnostics HTTP server.
pub mod diagnostics;
pub mod enrichment;
/// Predicate pushdown hints flowing from the SQL transform to input sources.
pub mod filter_hints;
pub mod format;
pub mod framer;
/// Input source trait and file-backed implementation.
pub mod input;
pub mod otlp;
/// Scan configuration types and fast number parsers.
pub mod scan_config;
/// Generic JSON-to-columnar scan loop and the `ScanBuilder` trait.
pub mod scanner;
pub mod tail;
