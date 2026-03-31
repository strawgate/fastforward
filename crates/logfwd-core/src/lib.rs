#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod aggregator;
pub mod byte_search;
pub mod cri;
pub mod framer;
pub mod otlp;
pub mod scan_config;
pub mod scanner;
/// Streaming SIMD structural character detection.
pub mod structural;
