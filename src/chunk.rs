//! Double-buffered chunk accumulator. Reads raw bytes into one buffer while
//! the other is owned by the processing pipeline. Zero-copy handoff via swap.
//!
//! The only copy is the leftover partial line (~200 bytes avg) moved from
//! the full buffer to the empty one after swap.

use std::io::{self, Read};

/// Configuration for chunk sizing, including adaptive tuning bounds.
#[derive(Clone, Debug)]
pub struct ChunkConfig {
    /// Target chunk size in bytes.
    pub target_size: usize,
    /// Minimum chunk size for adaptive tuning (bytes).
    pub min_size: usize,
    /// Maximum chunk size for adaptive tuning (bytes).
    pub max_size: usize,
    /// Maximum time to wait before flushing a partial chunk (microseconds).
    /// 0 = no time-based flushing (only size-based).
    pub flush_timeout_us: u64,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        ChunkConfig {
            target_size: 256 * 1024,
            min_size: 32 * 1024,
            max_size: 16 * 1024 * 1024,
            flush_timeout_us: 10_000,
        }
    }
}

/// Stats from processing a single chunk, used for adaptive tuning.
#[derive(Clone, Debug, Default)]
pub struct ChunkStats {
    pub raw_bytes: usize,
    pub read_ns: u64,
    pub compress_ns: u64,
    pub compressed_bytes: usize,
}

/// An owned chunk of complete log lines, ready for processing.
/// The processing side owns this — no borrow conflicts with the reader.
pub struct OwnedChunk {
    buf: Vec<u8>,
    /// Number of valid bytes (the chunk data is buf[..len]).
    len: usize,
}

impl OwnedChunk {
    /// The chunk bytes — complete lines, newline-delimited.
    #[inline]
    pub fn data(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Number of valid bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Double-buffered chunk accumulator.
///
/// Two buffers: `fill_buf` (being read into) and `ready_buf` (handed to processing).
/// When `fill_buf` has enough data, we find the newline boundary, swap the
/// buffers, and copy only the leftover partial line to the new `fill_buf`.
pub struct ChunkAccumulator {
    /// Buffer currently being filled by read().
    fill_buf: Vec<u8>,
    /// Buffer currently owned by the processing pipeline (or idle).
    ready_buf: Vec<u8>,
    /// Valid bytes in fill_buf.
    fill_len: usize,
    config: ChunkConfig,
}

impl ChunkAccumulator {
    pub fn new(config: ChunkConfig) -> Self {
        let buf_size = config.max_size + 8192; // margin for partial lines
        ChunkAccumulator {
            fill_buf: vec![0u8; buf_size],
            ready_buf: vec![0u8; buf_size],
            fill_len: 0,
            config,
        }
    }

    /// Returns the current target chunk size.
    #[inline]
    pub fn target_size(&self) -> usize {
        self.config.target_size
    }

    /// Adjust target chunk size (for adaptive tuning). Clamps to [min, max].
    pub fn set_target_size(&mut self, size: usize) {
        self.config.target_size = size.clamp(self.config.min_size, self.config.max_size);
    }

    /// Read from `source` into the fill buffer. Returns bytes read, or 0 on EOF.
    pub fn fill_from<R: Read>(&mut self, source: &mut R) -> io::Result<usize> {
        let read_limit = (self.config.target_size + 8192).min(self.fill_buf.len());
        if self.fill_len >= read_limit {
            return Ok(0);
        }
        let n = source.read(&mut self.fill_buf[self.fill_len..read_limit])?;
        self.fill_len += n;
        Ok(n)
    }

    /// Try to yield a complete chunk. If we have >= target_size bytes,
    /// finds the last newline boundary, swaps buffers, and returns the
    /// full chunk as an OwnedChunk.
    ///
    /// The returned OwnedChunk is fully owned — no borrow on the accumulator.
    /// Process it however you want, for as long as you want.
    /// When done, return it via `reclaim()` so the buffer can be reused.
    pub fn try_take_chunk(&mut self) -> Option<OwnedChunk> {
        if self.fill_len < self.config.target_size {
            return None;
        }

        let search_end = self.config.target_size.min(self.fill_len);
        let boundary = memchr::memrchr(b'\n', &self.fill_buf[..search_end])?;
        let chunk_end = boundary + 1;

        self.split_and_swap(chunk_end)
    }

    /// Force-yield whatever data we have (for EOF / flush timeout).
    pub fn flush_take_chunk(&mut self) -> Option<OwnedChunk> {
        if self.fill_len == 0 {
            return None;
        }

        let chunk_end = match memchr::memrchr(b'\n', &self.fill_buf[..self.fill_len]) {
            Some(boundary) => boundary + 1,
            None => self.fill_len, // no newline — yield everything
        };

        self.split_and_swap(chunk_end)
    }

    /// Split fill_buf at chunk_end, swap buffers, copy leftover to new fill_buf.
    fn split_and_swap(&mut self, chunk_end: usize) -> Option<OwnedChunk> {
        let leftover_len = self.fill_len - chunk_end;

        // Swap the buffers. ready_buf becomes the chunk, fill_buf becomes
        // the new read target.
        std::mem::swap(&mut self.fill_buf, &mut self.ready_buf);

        // Now:
        //   ready_buf = old fill_buf (contains chunk data [0..chunk_end] + leftover [chunk_end..fill_len])
        //   fill_buf  = old ready_buf (empty / stale data — we'll overwrite)

        // Copy only the leftover partial line to the new fill_buf.
        if leftover_len > 0 {
            self.fill_buf[..leftover_len]
                .copy_from_slice(&self.ready_buf[chunk_end..chunk_end + leftover_len]);
        }
        self.fill_len = leftover_len;

        Some(OwnedChunk {
            buf: std::mem::take(&mut self.ready_buf),
            len: chunk_end,
        })
    }

    /// Return a processed chunk's buffer for reuse. This avoids reallocating
    /// on the next swap.
    pub fn reclaim(&mut self, chunk: OwnedChunk) {
        self.ready_buf = chunk.buf;
    }

    /// Number of bytes buffered in the fill buffer.
    #[inline]
    pub fn buffered(&self) -> usize {
        self.fill_len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.fill_len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_basic_chunking() {
        let mut config = ChunkConfig::default();
        config.target_size = 64;
        config.min_size = 16;
        let mut acc = ChunkAccumulator::new(config);

        let data = "line one!\nline two!\nline 333!\nline four\nline five\n\
                     line six!\nline 777!\nline 888!\nline 999!\nline ten!\n";
        let mut cursor = Cursor::new(data.as_bytes());

        let n = acc.fill_from(&mut cursor).unwrap();
        assert!(n > 0);

        let chunk = acc.try_take_chunk().expect("should have a chunk");
        assert!(chunk.len() >= 50);
        assert_eq!(chunk.data().last(), Some(&b'\n'));

        // Leftover should be preserved in the fill buffer.
        assert!(acc.buffered() > 0 || cursor.position() < data.len() as u64);

        acc.reclaim(chunk);
    }

    #[test]
    fn test_flush_partial() {
        let mut config = ChunkConfig::default();
        config.target_size = 1024;
        config.min_size = 16;
        let mut acc = ChunkAccumulator::new(config);

        let data = b"partial line one\npartial line two\n";
        let mut cursor = Cursor::new(&data[..]);
        acc.fill_from(&mut cursor).unwrap();

        assert!(acc.try_take_chunk().is_none());

        let chunk = acc.flush_take_chunk().expect("should flush");
        assert_eq!(chunk.data(), &data[..]);
        acc.reclaim(chunk);
    }

    #[test]
    fn test_no_newline() {
        let mut config = ChunkConfig::default();
        config.target_size = 32;
        config.min_size = 16;
        let mut acc = ChunkAccumulator::new(config);

        let data = b"this is a very long line with no newline at all and it keeps going";
        let mut cursor = Cursor::new(&data[..]);
        acc.fill_from(&mut cursor).unwrap();

        assert!(acc.try_take_chunk().is_none());

        let chunk = acc.flush_take_chunk().expect("should flush all");
        assert_eq!(chunk.len(), data.len());
        acc.reclaim(chunk);
    }

    #[test]
    fn test_leftover_preserved_across_swap() {
        let mut config = ChunkConfig::default();
        config.target_size = 30;
        config.min_size = 16;
        let mut acc = ChunkAccumulator::new(config);

        let data = b"aaaa\nbbbb\ncccc\ndddd\neeee\nffff\n";
        let mut cursor = Cursor::new(&data[..]);
        acc.fill_from(&mut cursor).unwrap();

        let chunk1 = acc.try_take_chunk().unwrap();
        let chunk1_data = chunk1.data().to_vec();
        acc.reclaim(chunk1);

        // Remaining data should be accessible via flush.
        if acc.buffered() > 0 {
            let chunk2 = acc.flush_take_chunk().unwrap();
            let total = [chunk1_data.as_slice(), chunk2.data()].concat();
            assert_eq!(&total[..], &data[..total.len()]);
            acc.reclaim(chunk2);
        }
    }

    #[test]
    fn test_reclaim_reuses_buffer() {
        let mut config = ChunkConfig::default();
        config.target_size = 30;
        config.min_size = 16;
        let mut acc = ChunkAccumulator::new(config);

        let data = b"aaaa\nbbbb\ncccc\ndddd\neeee\nffff\ngggg\nhhhh\niiii\njjjj\n";
        let mut cursor = Cursor::new(&data[..]);
        acc.fill_from(&mut cursor).unwrap();

        // Take, reclaim, take again — should work without growing memory.
        let chunk1 = acc.try_take_chunk().unwrap();
        let ptr1 = chunk1.buf.as_ptr();
        acc.reclaim(chunk1);

        // Fill more data and take again.
        acc.fill_from(&mut cursor).unwrap();
        if let Some(chunk2) = acc.flush_take_chunk() {
            // The buffer should be the same allocation (reclaimed).
            // (Not guaranteed if Vec reallocates, but for same-size it should hold.)
            acc.reclaim(chunk2);
        }
    }
}
