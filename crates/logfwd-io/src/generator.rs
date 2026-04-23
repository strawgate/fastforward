//! Synthetic data generator input source.
//!
//! Produces JSON log lines at a configurable rate. Used for benchmarking
//! and testing pipelines without external data sources.

include!("generator/types.rs");
include!("generator/logs.rs");
include!("generator/input.rs");
include!("generator/encoding.rs");

#[cfg(test)]
mod tests {
    include!("generator/tests/basic.rs");
    include!("generator/tests/timestamps.rs");
    include!("generator/tests/record_profile.rs");
}
