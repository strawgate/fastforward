use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use ffwd_types::pipeline::SourceId;

use super::state::MAX_TRUNCATION_GENERATION;

/// Byte offset within a file. Newtype prevents mixing with SourceId.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteOffset(pub u64);

/// Identity of a file based on device + inode + content fingerprint.
/// Survives renames. Detects inode reuse via fingerprint mismatch.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub fingerprint: u64,
}

impl FileIdentity {
    /// Derive a stable `SourceId` from the compound key (device, inode, fingerprint).
    ///
    /// Hashing all three fields prevents collisions between files that share
    /// the same first N bytes (same fingerprint) but live on different inodes.
    /// Two files on the same inode+device are the same file, so the fingerprint
    /// differentiates after inode reuse (e.g., log rotation).
    pub fn source_id(&self) -> SourceId {
        // Empty file sentinel: fingerprint 0 means no data to checkpoint.
        if self.fingerprint == 0 {
            return SourceId(0);
        }
        let primary = source_digest(self.device, self.inode, self.fingerprint, 0);
        let fallback = source_digest(self.device, self.inode, self.fingerprint, 1);
        source_id_from_digests(primary, fallback)
    }
}

/// Odd multiplier that spreads truncation generations across the id space.
///
/// Being odd makes it invertible modulo 2^64, so a stored generation id can be
/// mapped back to its generation number without probing (see
/// [`truncation_generation_of`]).
const GENERATION_STRIDE: u64 = 0x9E37_79B9_7F4A_7C15;

/// Multiplicative inverse of [`GENERATION_STRIDE`] modulo 2^64.
const GENERATION_STRIDE_INV: u64 = modular_inverse_u64(GENERATION_STRIDE);

/// Newton iteration for the inverse of an odd `u64` modulo 2^64.
const fn modular_inverse_u64(value: u64) -> u64 {
    // Each step doubles the number of correct low bits: 3 -> 6 -> ... -> 96.
    let mut inverse = value;
    let mut step = 0;
    while step < 5 {
        inverse = inverse.wrapping_mul(2u64.wrapping_sub(value.wrapping_mul(inverse)));
        step += 1;
    }
    inverse
}

/// Return the `SourceId` of a file after `generation` detected truncations.
///
/// `base` is [`FileIdentity::source_id`]. Generation 0 is the base id itself,
/// so files that were never truncated keep the ids existing checkpoints use.
/// Each later generation is a distinct id, so a file that is truncated and
/// rewritten with the same leading bytes is checkpointed as a new source
/// instead of inheriting the offsets of its previous contents.
pub(super) fn truncation_generation_source_id(base: SourceId, generation: u64) -> SourceId {
    if base == SourceId(0) || generation == 0 {
        return base;
    }
    let id = base
        .0
        .wrapping_add(generation.wrapping_mul(GENERATION_STRIDE));
    // Keep SourceId(0) reserved for the empty-file sentinel. The odds of
    // landing here are 2^-64; the only cost is that a restore cannot decode
    // this generation and falls back to the base id.
    if id == 0 { SourceId(1) } else { SourceId(id) }
}

/// Decode which truncation generation of `base` produced `candidate`.
///
/// Returns `Some(generation)` when `candidate` equals
/// [`truncation_generation_source_id`]`(base, generation)` for a generation in
/// `0..=MAX_TRUNCATION_GENERATION`, and `None` otherwise.
pub(super) fn truncation_generation_of(base: SourceId, candidate: SourceId) -> Option<u64> {
    if base == SourceId(0) || candidate == SourceId(0) {
        return None;
    }
    let generation = candidate
        .0
        .wrapping_sub(base.0)
        .wrapping_mul(GENERATION_STRIDE_INV);
    (generation <= MAX_TRUNCATION_GENERATION).then_some(generation)
}

fn source_digest(device: u64, inode: u64, fingerprint: u64, seed: u64) -> u64 {
    let mut h = xxhash_rust::xxh64::Xxh64::new(seed);
    h.update(&device.to_le_bytes());
    h.update(&inode.to_le_bytes());
    h.update(&fingerprint.to_le_bytes());
    h.digest()
}

fn source_id_from_digests(primary: u64, fallback: u64) -> SourceId {
    // Reserve SourceId(0) as the empty-file sentinel.
    if primary != 0 {
        return SourceId(primary);
    }
    if fallback != 0 {
        return SourceId(fallback);
    }
    // Extremely unlikely: both seeds hashed to 0. Keep non-zero invariant.
    SourceId(1)
}

#[cfg(unix)]
fn metadata_device_inode(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn metadata_device_inode(_meta: &std::fs::Metadata) -> (u64, u64) {
    // Fallback for non-Unix builds. Windows can later switch to file-index based ids.
    (0, 0)
}

/// Compute the fingerprint of a file: xxhash64 of the first N bytes.
///
/// Preserves the caller's current file cursor position.
pub(super) fn compute_fingerprint(file: &mut File, max_bytes: usize) -> io::Result<u64> {
    let pos = file.stream_position()?;
    file.seek(SeekFrom::Start(0))?;

    let mut buf = vec![0u8; max_bytes];
    let mut total = 0;
    while total < buf.len() {
        match file.read(&mut buf[total..])? {
            0 => break,
            n => total += n,
        }
    }
    let n = total;

    file.seek(SeekFrom::Start(pos))?;

    if n == 0 {
        // Empty file gets a sentinel fingerprint.
        return Ok(0);
    }
    Ok(xxhash_rust::xxh64::xxh64(&buf[..n], 0))
}

/// Build a FileIdentity from an already-open file handle.
pub(super) fn identify_open_file(
    file: &mut File,
    fingerprint_bytes: usize,
) -> io::Result<FileIdentity> {
    let meta = file.metadata()?;
    let (device, inode) = metadata_device_inode(&meta);
    let fingerprint = compute_fingerprint(file, fingerprint_bytes)?;
    Ok(FileIdentity {
        device,
        inode,
        fingerprint,
    })
}

/// Build a FileIdentity for a path.
pub(super) fn identify_file(path: &Path, fingerprint_bytes: usize) -> io::Result<FileIdentity> {
    let mut file = File::open(path)?;
    identify_open_file(&mut file, fingerprint_bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        GENERATION_STRIDE, GENERATION_STRIDE_INV, MAX_TRUNCATION_GENERATION, compute_fingerprint,
        source_id_from_digests, truncation_generation_of, truncation_generation_source_id,
    };
    use ffwd_types::pipeline::SourceId;
    use std::io::Write;

    #[test]
    fn source_id_uses_primary_digest_when_nonzero() {
        assert_eq!(source_id_from_digests(42, 7), SourceId(42));
    }

    #[test]
    fn source_id_uses_fallback_digest_when_primary_is_zero() {
        assert_eq!(source_id_from_digests(0, 7), SourceId(7));
    }

    #[test]
    fn source_id_uses_nonzero_sentinel_when_both_digests_are_zero() {
        assert_eq!(source_id_from_digests(0, 0), SourceId(1));
    }

    #[test]
    fn generation_stride_inverse_is_exact() {
        assert_eq!(GENERATION_STRIDE.wrapping_mul(GENERATION_STRIDE_INV), 1);
    }

    #[test]
    fn generation_zero_keeps_base_source_id() {
        let base = SourceId(0x1234_5678_9ABC_DEF0);
        assert_eq!(truncation_generation_source_id(base, 0), base);
        assert_eq!(truncation_generation_of(base, base), Some(0));
    }

    #[test]
    fn truncation_generations_get_distinct_decodable_ids() {
        let base = SourceId(0x1234_5678_9ABC_DEF0);
        let mut seen = std::collections::HashSet::new();
        for generation in [0, 1, 2, 3, 1000, MAX_TRUNCATION_GENERATION] {
            let id = truncation_generation_source_id(base, generation);
            assert_ne!(id, SourceId(0));
            assert!(seen.insert(id), "generation {generation} reused an id");
            assert_eq!(truncation_generation_of(base, id), Some(generation));
        }
    }

    #[test]
    fn empty_file_sentinel_has_no_generations() {
        assert_eq!(truncation_generation_source_id(SourceId(0), 5), SourceId(0));
        assert_eq!(truncation_generation_of(SourceId(0), SourceId(7)), None);
    }

    #[test]
    fn unrelated_source_id_does_not_decode_as_generation() {
        let base = SourceId(0x1234_5678_9ABC_DEF0);
        let other = SourceId(0x0FED_CBA9_8765_4321);
        assert_eq!(truncation_generation_of(base, other), None);
    }

    #[test]
    fn far_generation_id_is_not_decoded() {
        // Decodes as generation 81,837,719, past the bound, so a stale
        // checkpoint from another file cannot be adopted.
        let base = SourceId(0xd10c_ce99_2cf0_6c9f);
        let unrelated = SourceId(0xb869_d299_7057_3302);
        assert_eq!(truncation_generation_of(base, unrelated), None);
    }

    #[test]
    fn compute_fingerprint_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.log");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(b"hello world, this is a test line\n").unwrap();
        }
        let mut file = std::fs::File::open(&path).unwrap();
        let fp1 = compute_fingerprint(&mut file, 256).unwrap();
        let fp2 = compute_fingerprint(&mut file, 256).unwrap();
        assert_eq!(fp1, fp2, "consecutive fingerprint calls must be identical");
        assert_ne!(
            fp1, 0,
            "non-empty file must not produce sentinel fingerprint"
        );
    }

    #[test]
    fn compute_fingerprint_empty_file_returns_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.log");
        std::fs::File::create(&path).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let fp = compute_fingerprint(&mut file, 256).unwrap();
        assert_eq!(fp, 0, "empty file must return sentinel fingerprint 0");
    }
}
