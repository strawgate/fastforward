//! Composable framing wrapper for input sources.
//!
//! `FramedInput` wraps any [`InputSource`] and handles newline framing
//! (remainder management across polls) and format processing (CRI, Auto,
//! passthrough). The pipeline receives scanner-ready bytes without knowing
//! about formats or line boundaries.
//!
//! Remainder buffers are tracked per-source so that interleaved data from
//! multiple files (or TCP connections) never cross-contaminates partial lines.

use crate::diagnostics::ComponentStats;
use crate::filter_hints::FilterHints;
use crate::format::FormatProcessor;
use crate::input::{InputEvent, InputSource};
use crate::tail::ByteOffset;
use logfwd_core::pipeline::SourceId;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Maximum remainder buffer size before discarding (prevents OOM on
/// input without newlines). Applied per source.
const MAX_REMAINDER_BYTES: usize = 2 * 1024 * 1024;

/// Sentinel `SourceId` used for events that carry `source_id: None`
/// (push sources like generators, UDP, TCP without per-connection tracking).
const SENTINEL_SOURCE_ID: SourceId = SourceId(0);

/// Wraps a raw [`InputSource`] with newline framing and format processing.
///
/// The inner source provides raw bytes (from file, TCP, UDP, etc.). This
/// wrapper splits on newlines, manages partial-line remainders across polls,
/// and runs format-specific processing (CRI extraction, passthrough, etc.).
/// The output is scanner-ready bytes.
///
/// Remainder buffers are keyed by `SourceId` so that interleaved data from
/// multiple sources never mixes partial lines.
pub struct FramedInput {
    inner: Box<dyn InputSource>,
    format: FormatProcessor,
    /// Per-source remainder buffers. Keyed by SourceId; events with
    /// `source_id: None` use `SENTINEL_SOURCE_ID`.
    remainders: HashMap<SourceId, Vec<u8>>,
    out_buf: Vec<u8>,
    /// Spare buffer swapped in when out_buf is emitted, preserving capacity
    /// across polls without allocating.
    spare_buf: Vec<u8>,
    stats: Arc<ComponentStats>,
}

impl FramedInput {
    pub fn new(
        inner: Box<dyn InputSource>,
        format: FormatProcessor,
        stats: Arc<ComponentStats>,
    ) -> Self {
        Self {
            inner,
            format,
            remainders: HashMap::new(),
            out_buf: Vec::with_capacity(64 * 1024),
            spare_buf: Vec::with_capacity(64 * 1024),
            stats,
        }
    }

    /// Resolve `Option<SourceId>` to a concrete key for the remainders map.
    fn remainder_key(source_id: Option<SourceId>) -> SourceId {
        source_id.unwrap_or(SENTINEL_SOURCE_ID)
    }
}

impl InputSource for FramedInput {
    fn poll(&mut self) -> io::Result<Vec<InputEvent>> {
        let raw_events = self.inner.poll()?;
        if raw_events.is_empty() {
            return Ok(vec![]);
        }

        let mut result_events: Vec<InputEvent> = Vec::new();

        for event in raw_events {
            match event {
                InputEvent::Data { bytes, source_id } => {
                    self.stats.inc_bytes(bytes.len() as u64);

                    let key = Self::remainder_key(source_id);

                    // Prepend remainder from last poll for this source,
                    // reusing the Vec's capacity.
                    let mut chunk = self.remainders.remove(&key).unwrap_or_default();
                    chunk.extend_from_slice(&bytes);

                    // Find last newline — everything before is complete lines,
                    // everything after is the new remainder.
                    match memchr::memrchr(b'\n', &chunk) {
                        Some(pos) => {
                            if pos + 1 < chunk.len() {
                                // Move tail to remainder without allocating.
                                let tail = chunk.split_off(pos + 1);
                                if tail.len() > MAX_REMAINDER_BYTES {
                                    self.stats.inc_parse_errors(1);
                                    // Drop the oversized remainder.
                                    self.format.reset();
                                } else {
                                    self.remainders.insert(key, tail);
                                }
                                chunk.truncate(pos + 1);
                            }
                        }
                        None => {
                            // No newline at all — entire chunk is remainder.
                            if chunk.len() > MAX_REMAINDER_BYTES {
                                self.stats.inc_parse_errors(1);
                                self.format.reset();
                            } else {
                                self.remainders.insert(key, chunk);
                            }
                            continue;
                        }
                    }

                    // Process complete lines through format handler.
                    self.out_buf.clear();
                    self.format.process_lines(&chunk, &mut self.out_buf);

                    let line_count = memchr::memchr_iter(b'\n', &chunk).count();
                    self.stats.inc_lines(line_count as u64);

                    if !self.out_buf.is_empty() {
                        // Take out_buf's content, swap in spare_buf's capacity
                        // for next iteration. No allocation — the 64KB bounces
                        // between the two buffers.
                        let data = std::mem::take(&mut self.out_buf);
                        std::mem::swap(&mut self.out_buf, &mut self.spare_buf);
                        result_events.push(InputEvent::Data {
                            bytes: data,
                            source_id,
                        });
                    }
                }
                // Rotation/truncation: clear framing state + forward event.
                //
                // TODO: When InputEvent gains source_id for Rotated/Truncated,
                // clear only the affected source's remainder instead of all.
                event @ (InputEvent::Rotated | InputEvent::Truncated) => {
                    self.remainders.clear();
                    self.format.reset();
                    result_events.push(event);
                }
                // End of file: flush any partial-line remainder.
                //
                // When a file ends without a trailing newline the last record
                // sits in the remainder indefinitely.  Appending a synthetic
                // `\n` lets the format processor treat it as a complete line so
                // it reaches the scanner instead of being silently dropped.
                //
                // We flush ALL remainders on EOF because the current EndOfFile
                // event does not carry a source_id. When it does, we should
                // flush only the affected source.
                InputEvent::EndOfFile => {
                    // Collect keys to avoid borrowing issues.
                    let keys: Vec<SourceId> = self.remainders.keys().copied().collect();
                    for key in keys {
                        if let Some(mut remainder) = self.remainders.remove(&key) {
                            if !remainder.is_empty() {
                                remainder.push(b'\n');

                                self.out_buf.clear();
                                self.format.process_lines(&remainder, &mut self.out_buf);

                                self.stats.inc_lines(1);

                                if !self.out_buf.is_empty() {
                                    let data = std::mem::take(&mut self.out_buf);
                                    std::mem::swap(&mut self.out_buf, &mut self.spare_buf);
                                    let source_id = if key == SENTINEL_SOURCE_ID {
                                        None
                                    } else {
                                        Some(key)
                                    };
                                    result_events.push(InputEvent::Data {
                                        bytes: data,
                                        source_id,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(result_events)
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn apply_hints(&mut self, hints: &FilterHints) {
        self.inner.apply_hints(hints);
    }

    /// Return checkpoint offsets adjusted for buffered remainder bytes.
    ///
    /// The inner source reports the file read offset (bytes consumed from
    /// the OS). But some of those bytes are sitting in our remainder buffer
    /// and have NOT been emitted as framed data. Subtracting the remainder
    /// length gives the true "processed" offset -- the position a crash
    /// recovery should seek to in order to replay only unprocessed data.
    fn checkpoint_data(&self) -> Vec<(SourceId, ByteOffset)> {
        self.inner
            .checkpoint_data()
            .into_iter()
            .map(|(sid, offset)| {
                let remainder_len = self
                    .remainders
                    .get(&sid)
                    .map_or(0, |r| r.len() as u64);
                (sid, ByteOffset(offset.0.saturating_sub(remainder_len)))
            })
            .collect()
    }

    fn source_paths(&self) -> Vec<(SourceId, PathBuf)> {
        self.inner.source_paths()
    }

    fn set_offset(&mut self, path: &Path, offset: u64) {
        self.inner.set_offset(path, offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Mock input source for testing.
    struct MockSource {
        name: String,
        events: VecDeque<Vec<InputEvent>>,
        offsets: Vec<(SourceId, ByteOffset)>,
    }

    impl MockSource {
        fn new(batches: Vec<Vec<InputEvent>>) -> Self {
            Self {
                name: "mock".to_string(),
                events: batches.into(),
                offsets: vec![],
            }
        }

        fn from_chunks(chunks: Vec<&[u8]>) -> Self {
            Self::new(
                chunks
                    .into_iter()
                    .map(|c| {
                        vec![InputEvent::Data {
                            bytes: c.to_vec(),
                            source_id: None,
                        }]
                    })
                    .collect(),
            )
        }

        fn with_offsets(mut self, offsets: Vec<(SourceId, ByteOffset)>) -> Self {
            self.offsets = offsets;
            self
        }
    }

    impl InputSource for MockSource {
        fn poll(&mut self) -> io::Result<Vec<InputEvent>> {
            Ok(self.events.pop_front().unwrap_or_default())
        }

        fn name(&self) -> &str {
            &self.name
        }

        fn checkpoint_data(&self) -> Vec<(SourceId, ByteOffset)> {
            self.offsets.clone()
        }
    }

    fn make_stats() -> Arc<ComponentStats> {
        Arc::new(ComponentStats::new())
    }

    fn collect_data(events: Vec<InputEvent>) -> Vec<u8> {
        let mut out = Vec::new();
        for e in events {
            if let InputEvent::Data { bytes, .. } = e {
                out.extend_from_slice(&bytes);
            }
        }
        out
    }

    #[test]
    fn passthrough_complete_lines() {
        let stats = make_stats();
        let source = MockSource::from_chunks(vec![b"line1\nline2\n"]);
        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(Arc::clone(&stats)),
            stats,
        );

        let events = framed.poll().unwrap();
        assert_eq!(collect_data(events), b"line1\nline2\n");
    }

    #[test]
    fn remainder_across_polls() {
        let stats = make_stats();
        let source = MockSource::from_chunks(vec![b"hello\nwor", b"ld\n"]);
        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(Arc::clone(&stats)),
            stats,
        );

        // First poll: "hello\n" is complete, "wor" is remainder
        let events1 = framed.poll().unwrap();
        assert_eq!(collect_data(events1), b"hello\n");

        // Second poll: remainder "wor" + "ld\n" → "world\n"
        let events2 = framed.poll().unwrap();
        assert_eq!(collect_data(events2), b"world\n");
    }

    #[test]
    fn no_newline_becomes_remainder() {
        let stats = make_stats();
        let source = MockSource::from_chunks(vec![b"partial", b"more\n"]);
        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(Arc::clone(&stats)),
            Arc::clone(&stats),
        );

        // First poll: no newline, everything goes to remainder
        let events1 = framed.poll().unwrap();
        assert!(collect_data(events1).is_empty());

        // Second poll: remainder + new data → complete line
        let events2 = framed.poll().unwrap();
        assert_eq!(collect_data(events2), b"partialmore\n");
    }

    #[test]
    fn remainder_capped_at_max() {
        let stats = make_stats();
        // Send > 2 MiB without a newline
        let big = vec![b'x'; MAX_REMAINDER_BYTES + 1];
        let source = MockSource::from_chunks(vec![&big]);
        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(Arc::clone(&stats)),
            Arc::clone(&stats),
        );

        let events = framed.poll().unwrap();
        assert!(collect_data(events).is_empty());
        assert_eq!(
            stats
                .parse_errors_total
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn tail_after_newline_is_capped_at_max() {
        let stats = make_stats();
        let mut chunk = b"ok\n".to_vec();
        chunk.extend(vec![b'x'; MAX_REMAINDER_BYTES + 1]);
        let source = MockSource::from_chunks(vec![&chunk]);
        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(Arc::clone(&stats)),
            Arc::clone(&stats),
        );

        let events = framed.poll().unwrap();
        assert_eq!(collect_data(events), b"ok\n");
        assert_eq!(
            stats
                .parse_errors_total
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn rotated_clears_remainder_and_format() {
        let stats = make_stats();
        let source = MockSource::new(vec![
            vec![InputEvent::Data {
                bytes: b"partial".to_vec(),
                source_id: None,
            }],
            vec![InputEvent::Rotated],
            vec![InputEvent::Data {
                bytes: b"fresh\n".to_vec(),
                source_id: None,
            }],
        ]);
        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(Arc::clone(&stats)),
            stats,
        );

        // Partial goes to remainder
        let _ = framed.poll().unwrap();

        // Rotation clears remainder
        let events2 = framed.poll().unwrap();
        assert!(events2.iter().any(|e| matches!(e, InputEvent::Rotated)));

        // Fresh data starts clean (no stale "partial" prefix)
        let events3 = framed.poll().unwrap();
        assert_eq!(collect_data(events3), b"fresh\n");
    }

    #[test]
    fn cri_format_extracts_messages() {
        let stats = make_stats();
        let input = b"2024-01-15T10:30:00Z stdout F {\"msg\":\"hello\"}\n";
        let source = MockSource::from_chunks(vec![input.as_slice()]);
        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::cri(2 * 1024 * 1024, Arc::clone(&stats)),
            stats,
        );

        let events = framed.poll().unwrap();
        assert_eq!(
            collect_data(events),
            b"{\"_timestamp\":\"2024-01-15T10:30:00Z\",\"_stream\":\"stdout\",\"msg\":\"hello\"}\n"
        );
    }

    #[test]
    fn split_anywhere_produces_same_output() {
        let stats = make_stats();
        let full_input = b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";

        // Reference: process entire input at once
        let source_full = MockSource::from_chunks(vec![full_input.as_slice()]);
        let mut framed_full = FramedInput::new(
            Box::new(source_full),
            FormatProcessor::passthrough(Arc::clone(&stats)),
            Arc::clone(&stats),
        );
        let reference = collect_data(framed_full.poll().unwrap());

        // Split at every possible byte position
        for split_at in 1..full_input.len() {
            let stats2 = make_stats();
            let chunk1 = &full_input[..split_at];
            let chunk2 = &full_input[split_at..];
            let source = MockSource::from_chunks(vec![chunk1, chunk2]);
            let mut framed = FramedInput::new(
                Box::new(source),
                FormatProcessor::passthrough(Arc::clone(&stats2)),
                stats2,
            );

            let mut collected = collect_data(framed.poll().unwrap());
            collected.extend_from_slice(&collect_data(framed.poll().unwrap()));

            assert_eq!(
                collected, reference,
                "split at byte {split_at} produced different output"
            );
        }
    }

    /// A file (or any source) that ends without a trailing newline must not
    /// silently drop its last record.  The `EndOfFile` event causes
    /// `FramedInput` to flush the remainder buffer with a synthetic newline.
    #[test]
    fn eof_flushes_remainder() {
        let stats = make_stats();
        let source = MockSource::new(vec![
            vec![InputEvent::Data {
                bytes: b"no-newline".to_vec(),
                source_id: None,
            }],
            vec![InputEvent::EndOfFile],
        ]);
        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(stats.clone()),
            stats,
        );

        // First poll: data with no newline — goes to remainder, nothing emitted.
        let events1 = framed.poll().unwrap();
        assert!(collect_data(events1).is_empty());

        // Second poll: EndOfFile flushes the remainder as a complete line.
        let events2 = framed.poll().unwrap();
        assert_eq!(collect_data(events2), b"no-newline\n");
    }

    /// Multiple records in a file where only the last one lacks a newline:
    /// all records must be emitted.
    #[test]
    fn eof_flushes_only_partial_remainder() {
        let stats = make_stats();
        let source = MockSource::new(vec![
            vec![InputEvent::Data {
                bytes: b"complete\npartial".to_vec(),
                source_id: None,
            }],
            vec![InputEvent::EndOfFile],
        ]);
        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(stats.clone()),
            stats,
        );

        // First poll: "complete\n" is emitted; "partial" stays in remainder.
        let events1 = framed.poll().unwrap();
        assert_eq!(collect_data(events1), b"complete\n");

        // Second poll: EndOfFile flushes "partial" with a synthetic newline.
        let events2 = framed.poll().unwrap();
        assert_eq!(collect_data(events2), b"partial\n");
    }

    /// A redundant EndOfFile (no bytes in remainder) must produce no output.
    #[test]
    fn eof_with_empty_remainder_is_noop() {
        let stats = make_stats();
        let source = MockSource::new(vec![
            vec![InputEvent::Data {
                bytes: b"line\n".to_vec(),
                source_id: None,
            }],
            vec![InputEvent::EndOfFile],
        ]);
        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(stats.clone()),
            stats,
        );

        let events1 = framed.poll().unwrap();
        assert_eq!(collect_data(events1), b"line\n");

        let events2 = framed.poll().unwrap();
        assert!(collect_data(events2).is_empty());
    }

    // -----------------------------------------------------------------------
    // Per-source remainder tests
    // -----------------------------------------------------------------------

    /// Two sources interleaving with partial lines -- no cross-contamination.
    #[test]
    fn two_sources_interleaved_no_cross_contamination() {
        let stats = make_stats();
        let sid_a = SourceId(100);
        let sid_b = SourceId(200);

        let source = MockSource::new(vec![
            // Poll 1: partial lines from both sources in one batch
            vec![
                InputEvent::Data {
                    bytes: b"hello-from-A".to_vec(),
                    source_id: Some(sid_a),
                },
                InputEvent::Data {
                    bytes: b"hello-from-B".to_vec(),
                    source_id: Some(sid_b),
                },
            ],
            // Poll 2: complete the lines from each source
            vec![
                InputEvent::Data {
                    bytes: b"-done\n".to_vec(),
                    source_id: Some(sid_a),
                },
                InputEvent::Data {
                    bytes: b"-done\n".to_vec(),
                    source_id: Some(sid_b),
                },
            ],
        ]);

        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(Arc::clone(&stats)),
            stats,
        );

        // Poll 1: no complete lines -- everything in remainders.
        let events1 = framed.poll().unwrap();
        assert!(collect_data(events1).is_empty());

        // Poll 2: each source gets its own remainder prepended.
        let events2 = framed.poll().unwrap();
        let mut output_a = Vec::new();
        let mut output_b = Vec::new();
        for e in events2 {
            if let InputEvent::Data { bytes, source_id } = e {
                match source_id {
                    Some(sid) if sid == sid_a => output_a.extend_from_slice(&bytes),
                    Some(sid) if sid == sid_b => output_b.extend_from_slice(&bytes),
                    _ => panic!("unexpected source_id"),
                }
            }
        }
        assert_eq!(output_a, b"hello-from-A-done\n");
        assert_eq!(output_b, b"hello-from-B-done\n");
    }

    /// Truncation clears all remainders (current behavior).
    #[test]
    fn truncation_clears_all_remainders() {
        let stats = make_stats();
        let sid_a = SourceId(100);
        let sid_b = SourceId(200);

        let source = MockSource::new(vec![
            // Partial lines from two sources
            vec![
                InputEvent::Data {
                    bytes: b"partial-A".to_vec(),
                    source_id: Some(sid_a),
                },
                InputEvent::Data {
                    bytes: b"partial-B".to_vec(),
                    source_id: Some(sid_b),
                },
            ],
            // Truncation
            vec![InputEvent::Truncated],
            // Fresh data from source A
            vec![InputEvent::Data {
                bytes: b"fresh-A\n".to_vec(),
                source_id: Some(sid_a),
            }],
        ]);

        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(Arc::clone(&stats)),
            stats,
        );

        // Partials go into remainders
        let _ = framed.poll().unwrap();

        // Truncation clears all
        let _ = framed.poll().unwrap();

        // Fresh data from A -- must NOT include "partial-A" prefix
        let events3 = framed.poll().unwrap();
        assert_eq!(collect_data(events3), b"fresh-A\n");
    }

    /// checkpoint_data() subtracts remainder length from inner offsets.
    #[test]
    fn checkpoint_data_subtracts_remainder() {
        let stats = make_stats();
        let sid = SourceId(42);

        // The inner source reports offset 1000 for our source.
        let source = MockSource::new(vec![vec![InputEvent::Data {
            bytes: b"hello\nwor".to_vec(),
            source_id: Some(sid),
        }]])
        .with_offsets(vec![(sid, ByteOffset(1000))]);

        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(Arc::clone(&stats)),
            stats,
        );

        // After poll, "wor" (3 bytes) is in the remainder for sid.
        let _ = framed.poll().unwrap();

        let cp = framed.checkpoint_data();
        assert_eq!(cp.len(), 1);
        assert_eq!(cp[0].0, sid);
        // 1000 - 3 = 997
        assert_eq!(cp[0].1, ByteOffset(997));
    }

    /// checkpoint_data() returns the raw offset when no remainder is buffered.
    #[test]
    fn checkpoint_data_no_remainder() {
        let stats = make_stats();
        let sid = SourceId(42);

        let source = MockSource::new(vec![vec![InputEvent::Data {
            bytes: b"complete\n".to_vec(),
            source_id: Some(sid),
        }]])
        .with_offsets(vec![(sid, ByteOffset(500))]);

        let mut framed = FramedInput::new(
            Box::new(source),
            FormatProcessor::passthrough(Arc::clone(&stats)),
            stats,
        );

        let _ = framed.poll().unwrap();

        let cp = framed.checkpoint_data();
        assert_eq!(cp.len(), 1);
        assert_eq!(cp[0].1, ByteOffset(500));
    }
}
