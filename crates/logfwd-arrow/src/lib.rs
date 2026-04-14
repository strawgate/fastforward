//! Arrow integration layer for logfwd.
//!
//! Implements logfwd-core's `ScanBuilder` trait using Apache Arrow types.
//! Contains `StreamingBuilder` (zero-copy hot path) and the `Scanner`
//! wrapper type that produces `RecordBatch`.

#[cfg(feature = "_test-internals")]
pub mod columnar;
#[cfg(not(feature = "_test-internals"))]
#[allow(dead_code)]
pub(crate) mod columnar;
pub mod conflict_schema;
pub mod materialize;
pub mod scanner;
pub mod star_schema;
pub mod streaming_builder;

/// Check and set a per-row written bit for the given field index.
///
/// Returns `true` if the field has already been written in this row
/// (duplicate key — caller should skip the write), `false` otherwise.
///
/// # Limitation
/// Only the first 64 fields (indices 0–63) are tracked. For `idx >= 64` this
/// function always returns `false`, meaning **duplicate-key detection is
/// disabled** for those fields. The first value written wins by convention;
/// subsequent writes for the same key in the same row are silently accepted
/// rather than deduplicated. This matches RFC 8259's "undefined" semantics for
/// duplicate keys and keeps the hot path allocation-free.
#[inline(always)]
pub(crate) fn check_dup_bits(written_bits: &mut u64, idx: usize) -> bool {
    if idx >= u64::BITS as usize {
        // Duplicate-key detection is not tracked beyond the first 64 fields.
        // Writes for idx >= 64 are always allowed through.
        return false;
    }
    let bit = 1u64 << idx;
    if *written_bits & bit != 0 {
        return true;
    }
    *written_bits |= bit;
    false
}

// Re-export scanner types for convenience
pub use scanner::Scanner;
pub use streaming_builder::StreamingBuilder;

#[cfg(kani)]
mod verification {
    use super::*;

    /// Exhaustive proof: first call for a tracked index clears, second detects duplicate.
    #[kani::proof]
    fn verify_check_dup_bits_first_write_then_duplicate() {
        let idx: usize = kani::any();
        kani::assume(idx < 128);
        let mut bits: u64 = 0;

        let first = check_dup_bits(&mut bits, idx);
        let second = check_dup_bits(&mut bits, idx);

        if idx < 64 {
            assert!(!first, "first write must not be flagged as duplicate");
            assert!(second, "second write must be flagged as duplicate");
            assert!(bits & (1u64 << idx) != 0, "bit must be set after write");
        } else {
            assert!(!first, "idx >= 64 always returns false");
            assert!(!second, "idx >= 64 always returns false");
            assert_eq!(bits, 0, "no bits modified for idx >= 64");
        }

        kani::cover!(idx < 64, "tracked field");
        kani::cover!(idx >= 64, "untracked field");
    }

    /// Two distinct tracked indices never interfere with each other.
    #[kani::proof]
    fn verify_check_dup_bits_independence() {
        let a: usize = kani::any();
        let b: usize = kani::any();
        kani::assume(a < 64 && b < 64 && a != b);

        let mut bits: u64 = 0;
        let first_a = check_dup_bits(&mut bits, a);
        let first_b = check_dup_bits(&mut bits, b);
        let dup_a = check_dup_bits(&mut bits, a);
        let dup_b = check_dup_bits(&mut bits, b);

        assert!(!first_a, "a first write is clean");
        assert!(!first_b, "b first write is clean");
        assert!(dup_a, "a second write is duplicate");
        assert!(dup_b, "b second write is duplicate");
    }

    /// Pre-existing bits are preserved — setting idx doesn't clear other bits.
    #[kani::proof]
    fn verify_check_dup_bits_preserves_existing() {
        let idx: usize = kani::any();
        kani::assume(idx < 64);
        let mut bits: u64 = kani::any();
        let before = bits;

        let _ = check_dup_bits(&mut bits, idx);

        // All previously set bits remain set.
        assert_eq!(bits & before, before, "existing bits must be preserved");
        // The new bit is now set.
        assert!(bits & (1u64 << idx) != 0, "target bit must be set");
    }
}
