//! Shared deterministic data generators and helpers for logfwd benchmarks.
//!
//! All generators accept a `seed` parameter for reproducible output. Given the
//! same `(count, seed)` pair, every call returns byte-identical results.

pub mod generators;
