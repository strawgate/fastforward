//! Format parsers: convert raw input bytes into newline-delimited JSON.
//!
//! Each format (CRI, JSON, Raw) implements the [`FormatParser`] trait.
//! The pipeline feeds raw bytes in and gets back newline-terminated JSON
//! lines suitable for the SIMD scanner.

use crate::cri::{self, CriReassembler};

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Converts raw input bytes into newline-delimited JSON lines.
///
/// Implementations carry state across calls (partial lines, CRI reassembly).
/// All methods are called from a single thread — no `Sync` required.
pub trait FormatParser: Send {
    /// Process a chunk of raw bytes, appending complete newline-delimited
    /// JSON lines to `out`. Returns the number of lines produced.
    fn process(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> usize;

    /// Reset internal state (partial line buffers, CRI reassembly).
    /// Called on file rotation or truncation.
    fn reset(&mut self);

    /// Return and reset the count of bytes dropped due to partial-buffer
    /// overflow since the last call. Called by the pipeline after each poll
    /// cycle to accumulate the `logfwd_buffer_overflow_total` counter.
    fn take_overflows(&mut self) -> u64;
}

// ---------------------------------------------------------------------------
// JSON / Auto
// ---------------------------------------------------------------------------

/// Passes through newline-delimited JSON, carrying partial lines across calls.
pub struct JsonParser {
    partial: Vec<u8>,
    max_partial_bytes: usize,
    partial_overflows: u64,
}

impl JsonParser {
    /// Create a parser with no limit on partial-line buffering.
    pub fn new() -> Self {
        Self {
            partial: Vec::new(),
            max_partial_bytes: usize::MAX,
            partial_overflows: 0,
        }
    }

    /// Create a parser that drops partial lines exceeding `max_partial_bytes`
    /// and records the dropped byte count via [`FormatParser::take_overflows`].
    pub fn with_max_partial(max_partial_bytes: usize) -> Self {
        Self {
            partial: Vec::new(),
            max_partial_bytes,
            partial_overflows: 0,
        }
    }
}

impl Default for JsonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatParser for JsonParser {
    fn process(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> usize {
        let mut count = 0;
        let mut start = 0;
        for pos in memchr::memchr_iter(b'\n', bytes) {
            if self.partial.is_empty() {
                let line = &bytes[start..pos];
                if !line.is_empty() {
                    out.extend_from_slice(line);
                    out.push(b'\n');
                    count += 1;
                }
            } else {
                self.partial.extend_from_slice(&bytes[start..pos]);
                if !self.partial.is_empty() {
                    out.extend_from_slice(&self.partial);
                    out.push(b'\n');
                    count += 1;
                }
                self.partial.clear();
            }
            start = pos + 1;
        }
        if start < bytes.len() {
            self.partial.extend_from_slice(&bytes[start..]);
            if self.partial.len() > self.max_partial_bytes {
                self.partial_overflows += self.partial.len() as u64;
                self.partial.clear();
            }
        }
        count
    }

    fn reset(&mut self) {
        self.partial.clear();
    }

    fn take_overflows(&mut self) -> u64 {
        let n = self.partial_overflows;
        self.partial_overflows = 0;
        n
    }
}

// ---------------------------------------------------------------------------
// Raw
// ---------------------------------------------------------------------------

/// Wraps each line as `{"_raw":"<escaped>"}\n`.
pub struct RawParser {
    partial: Vec<u8>,
    max_partial_bytes: usize,
    partial_overflows: u64,
}

impl RawParser {
    /// Create a parser with no limit on partial-line buffering.
    pub fn new() -> Self {
        Self {
            partial: Vec::new(),
            max_partial_bytes: usize::MAX,
            partial_overflows: 0,
        }
    }

    /// Create a parser that drops partial lines exceeding `max_partial_bytes`
    /// and records the dropped byte count via [`FormatParser::take_overflows`].
    pub fn with_max_partial(max_partial_bytes: usize) -> Self {
        Self {
            partial: Vec::new(),
            max_partial_bytes,
            partial_overflows: 0,
        }
    }
}

impl Default for RawParser {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatParser for RawParser {
    fn process(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> usize {
        let mut count = 0;
        let mut start = 0;
        for pos in memchr::memchr_iter(b'\n', bytes) {
            let line = if self.partial.is_empty() {
                &bytes[start..pos]
            } else {
                self.partial.extend_from_slice(&bytes[start..pos]);
                self.partial.as_slice()
            };

            if !line.is_empty() {
                out.extend_from_slice(b"{\"_raw\":\"");
                for &b in line {
                    match b {
                        b'"' => out.extend_from_slice(b"\\\""),
                        b'\\' => out.extend_from_slice(b"\\\\"),
                        b'\n' => out.extend_from_slice(b"\\n"),
                        b'\r' => out.extend_from_slice(b"\\r"),
                        b'\t' => out.extend_from_slice(b"\\t"),
                        b if b < 0x20 => {
                            // Escape control characters per RFC 8259.
                            let _ = std::io::Write::write_fmt(out, format_args!("\\u{:04x}", b));
                        }
                        _ => out.push(b),
                    }
                }
                out.extend_from_slice(b"\"}\n");
                count += 1;
            }

            if !self.partial.is_empty() {
                self.partial.clear();
            }
            start = pos + 1;
        }
        if start < bytes.len() {
            self.partial.extend_from_slice(&bytes[start..]);
            if self.partial.len() > self.max_partial_bytes {
                self.partial_overflows += self.partial.len() as u64;
                self.partial.clear();
            }
        }
        count
    }

    fn reset(&mut self) {
        self.partial.clear();
    }

    fn take_overflows(&mut self) -> u64 {
        let n = self.partial_overflows;
        self.partial_overflows = 0;
        n
    }
}

// ---------------------------------------------------------------------------
// CRI
// ---------------------------------------------------------------------------

/// Parses CRI container log format, reassembles partial lines, and emits
/// the extracted message as a JSON line.
pub struct CriParser {
    reassembler: CriReassembler,
    /// Bytes from the previous chunk that did not end with a newline.
    partial: Vec<u8>,
    max_partial_bytes: usize,
    partial_overflows: u64,
}

impl CriParser {
    /// Create a parser with no limit on partial-line buffering.
    pub fn new(max_line_size: usize) -> Self {
        CriParser {
            reassembler: CriReassembler::new(max_line_size),
            partial: Vec::new(),
            max_partial_bytes: usize::MAX,
            partial_overflows: 0,
        }
    }

    /// Create a parser that drops partial lines exceeding `max_partial_bytes`
    /// and records the dropped byte count via [`FormatParser::take_overflows`].
    pub fn with_max_partial(max_line_size: usize, max_partial_bytes: usize) -> Self {
        CriParser {
            reassembler: CriReassembler::new(max_line_size),
            partial: Vec::new(),
            max_partial_bytes,
            partial_overflows: 0,
        }
    }
}

impl FormatParser for CriParser {
    fn process(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> usize {
        self.partial.extend_from_slice(bytes);

        if self.partial.len() > self.max_partial_bytes {
            self.partial_overflows += self.partial.len() as u64;
            self.partial.clear();
            return 0;
        }

        let Some(last_nl) = memchr::memrchr(b'\n', &self.partial) else {
            // No complete line yet — keep buffering.
            return 0;
        };

        let process_end = last_nl + 1;
        let count = cri::process_cri_to_buf(
            &self.partial[..process_end],
            &mut self.reassembler,
            None,
            out,
        );
        self.partial.drain(..process_end);
        count
    }

    fn reset(&mut self) {
        self.reassembler.reset();
        self.partial.clear();
    }

    fn take_overflows(&mut self) -> u64 {
        let n = self.partial_overflows;
        self.partial_overflows = 0;
        n
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_basic() {
        let mut parser = JsonParser::new();
        let mut out = Vec::new();
        let n = parser.process(b"{\"a\":1}\n{\"b\":2}\n", &mut out);
        assert_eq!(n, 2);
        assert_eq!(out, b"{\"a\":1}\n{\"b\":2}\n");
    }

    #[test]
    fn json_partial_carry() {
        let mut parser = JsonParser::new();
        let mut out = Vec::new();

        let n1 = parser.process(b"{\"a\":1}\n{\"b\":", &mut out);
        assert_eq!(n1, 1);

        let n2 = parser.process(b"2}\n", &mut out);
        assert_eq!(n2, 1);

        assert_eq!(out, b"{\"a\":1}\n{\"b\":2}\n");
    }

    #[test]
    fn json_reset_clears_partial() {
        let mut parser = JsonParser::new();
        let mut out = Vec::new();

        parser.process(b"{\"partial\":", &mut out);
        assert_eq!(out.len(), 0);

        parser.reset();
        let n = parser.process(b"{\"fresh\":1}\n", &mut out);
        assert_eq!(n, 1);
        assert_eq!(out, b"{\"fresh\":1}\n");
    }

    #[test]
    fn raw_basic() {
        let mut parser = RawParser::new();
        let mut out = Vec::new();
        let n = parser.process(b"plain line\nanother\n", &mut out);
        assert_eq!(n, 2);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("{\"_raw\":\"plain line\"}\n"));
        assert!(s.contains("{\"_raw\":\"another\"}\n"));
    }

    #[test]
    fn raw_escaping() {
        let mut parser = RawParser::new();
        let mut out = Vec::new();
        let n = parser.process(b"has \"quotes\" and \\backslash\n", &mut out);
        assert_eq!(n, 1);
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains(r#"{"_raw":"has \"quotes\" and \\backslash"}"#),
            "got: {s}"
        );
    }

    #[test]
    fn cri_basic() {
        let mut parser = CriParser::new(2 * 1024 * 1024);
        let mut out = Vec::new();
        let input = b"2024-01-15T10:30:00Z stdout F {\"msg\":\"hello\"}\n";
        let n = parser.process(input, &mut out);
        assert_eq!(n, 1);
        assert!(out.ends_with(b"\n"));
    }

    #[test]
    fn cri_partial_carry_at_chunk_boundary() {
        // A CRI line split across two read buffers must be reassembled.
        let mut parser = CriParser::new(2 * 1024 * 1024);
        let mut out = Vec::new();

        // First chunk has no newline — nothing should be emitted yet.
        let chunk1 = b"2024-01-15T10:30:00Z stdout F {\"msg\":\"hel";
        let n1 = parser.process(chunk1, &mut out);
        assert_eq!(n1, 0);
        assert_eq!(out.len(), 0);

        // Second chunk completes the line.
        let chunk2 = b"lo\"}\n";
        let n2 = parser.process(chunk2, &mut out);
        assert_eq!(n2, 1);
        assert!(out.ends_with(b"\n"));
        // The reassembled message must contain the full payload.
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("hello"), "got: {s}");
    }

    #[test]
    fn cri_partial_carry_multiple_chunks() {
        // Three chunks, line only complete at end of third.
        let mut parser = CriParser::new(2 * 1024 * 1024);
        let mut out = Vec::new();

        assert_eq!(parser.process(b"2024-01-15T10:30:00Z ", &mut out), 0);
        assert_eq!(parser.process(b"stdout F line_content", &mut out), 0);
        assert_eq!(out.len(), 0);

        let n = parser.process(b"\n", &mut out);
        assert_eq!(n, 1);
        assert!(out.ends_with(b"\n"));
    }

    #[test]
    fn cri_partial_remainder_after_newline() {
        // Chunk contains a complete line followed by a partial one.
        let mut parser = CriParser::new(2 * 1024 * 1024);
        let mut out = Vec::new();

        // First chunk: one complete line + start of a second (no trailing newline).
        let chunk1 =
            b"2024-01-15T10:30:00Z stdout F {\"n\":1}\n2024-01-15T10:30:01Z stdout F {\"n\":2}";
        let n1 = parser.process(chunk1, &mut out);
        assert_eq!(n1, 1);

        // Second chunk: just the closing newline.
        let n2 = parser.process(b"\n", &mut out);
        assert_eq!(n2, 1);

        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\"n\":1"), "got: {s}");
        assert!(s.contains("\"n\":2"), "got: {s}");
    }

    #[test]
    fn cri_reset_clears_partial() {
        let mut parser = CriParser::new(2 * 1024 * 1024);
        let mut out = Vec::new();

        // Feed a partial chunk (no newline) to populate the partial buffer.
        parser.process(b"2024-01-15T10:30:00Z stdout F incomplete", &mut out);
        assert_eq!(out.len(), 0);

        // After reset the stale partial is discarded.
        parser.reset();
        let n = parser.process(b"2024-01-15T10:30:01Z stdout F {\"fresh\":1}\n", &mut out);
        assert_eq!(n, 1);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("fresh"), "got: {s}");
    }

    // -- Overflow / buffer-bound tests ----------------------------------------

    #[test]
    fn json_partial_overflow_drops_and_records() {
        // Set a tiny partial limit so the first incomplete chunk triggers overflow.
        let mut parser = JsonParser::with_max_partial(5);
        let mut out = Vec::new();

        // Feed 6 bytes without a newline — exceeds max_partial_bytes=5.
        parser.process(b"123456", &mut out);
        assert_eq!(out.len(), 0, "overflow should produce no output");
        let overflows = parser.take_overflows();
        assert_eq!(overflows, 6, "expected 6 overflow bytes, got {overflows}");

        // Counter resets after take_overflows.
        assert_eq!(parser.take_overflows(), 0);

        // After overflow the partial buffer is cleared; normal lines still work.
        let n = parser.process(b"{\"ok\":1}\n", &mut out);
        assert_eq!(n, 1);
    }

    #[test]
    fn raw_partial_overflow_drops_and_records() {
        let mut parser = RawParser::with_max_partial(4);
        let mut out = Vec::new();

        // 5-byte partial without newline — exceeds limit of 4.
        parser.process(b"hello", &mut out);
        assert_eq!(out.len(), 0);
        let overflows = parser.take_overflows();
        assert_eq!(overflows, 5, "expected 5 overflow bytes, got {overflows}");

        // Normal line still processed after clear.
        let n = parser.process(b"ok\n", &mut out);
        assert_eq!(n, 1);
        assert_eq!(parser.take_overflows(), 0);
    }

    #[test]
    fn cri_partial_overflow_drops_and_records() {
        let max_line = 2 * 1024 * 1024;
        // Set max_partial_bytes just large enough for the valid CRI line below
        // (≈ 44 bytes including the newline) but smaller than the overflow input.
        let max_partial_bytes = 100;
        let mut parser = CriParser::with_max_partial(max_line, max_partial_bytes);
        let mut out = Vec::new();

        // 101 bytes without a newline — exceeds partial limit of 100.
        let overflow_input = [b'x'; 101];
        parser.process(&overflow_input, &mut out);
        assert_eq!(out.len(), 0);
        let overflows = parser.take_overflows();
        assert_eq!(
            overflows, 101,
            "expected 101 overflow bytes, got {overflows}"
        );

        // Normal CRI line still processed after clear.
        let line = b"2024-01-15T10:30:00Z stdout F {\"msg\":\"hi\"}\n";
        let n = parser.process(line, &mut out);
        assert_eq!(n, 1);
        assert_eq!(parser.take_overflows(), 0);
    }

    #[test]
    fn json_no_overflow_below_limit() {
        let mut parser = JsonParser::with_max_partial(100);
        let mut out = Vec::new();

        // Feed a partial that stays within the limit.
        parser.process(b"partial without newline", &mut out);
        assert_eq!(
            parser.take_overflows(),
            0,
            "no overflow expected within limit"
        );
    }
}
