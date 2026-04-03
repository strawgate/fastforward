# Per-Source Remainder & Checkpoint Coordination Design

Date: 2026-03-31
Context: Fix #797 (shared remainder corruption), #588 (checkpoint = processed
offset), and TCP/UDP per-connection framing. Builds on findings from
`file-tailing-audit.md` and `offset-checkpoint-research.md`.

## Problem Statement

Three related bugs share a root cause: the pipeline conflates data from
different sources and checkpoints bytes-read rather than bytes-processed.

1. **Shared remainder** (#797): `FramedInput` has one `remainder: Vec<u8>`.
   When a glob input tails multiple files, partial lines from file A
   corrupt data from file B.

2. **Checkpoint = read offset** (#588): `FileTailer.offset` advances when
   bytes are read from disk, but `FramedInput` may hold some of those bytes
   in its remainder buffer. On crash, those bytes are lost. The checkpoint
   should reflect the last byte that reached the scanner, not the last byte
   read from disk.

3. **TCP merged streams**: `TcpInput::poll()` concatenates bytes from all
   connections into one `all_data` Vec. A partial line from connection A
   is glued to the start of connection B's data.

## Design Overview

```text
Before:
  FileTailer ──► FramedInput(single remainder) ──► pipeline
  TcpInput   ──► FramedInput(single remainder) ──► pipeline

After:
  FileTailer ──► FramedInput(per-source remainders) ──► pipeline
                    │
                    └── reports consumed_bytes per source
                         │
                         ▼
                    checkpoint = read_start + consumed_bytes

  TcpInput(per-conn line_buf) ──► FramedInput(passthrough) ──► pipeline
```

---

## 1. SourceId Plumbing Through InputEvent

### Current InputEvent

```rust
pub enum InputEvent {
    Data { bytes: Vec<u8> },
    Rotated,
    Truncated,
    EndOfFile,
}
```

### New InputEvent

```rust
pub enum InputEvent {
    Data {
        bytes: Vec<u8>,
        /// Identity of the source that produced these bytes.
        /// `None` for inputs that have no per-source identity (UDP, generators).
        source_id: Option<SourceId>,
    },
    /// Source was rotated (file rotation, etc.).
    Rotated { source_id: Option<SourceId> },
    /// Source was truncated (copytruncate).
    Truncated { source_id: Option<SourceId> },
    /// Source has been fully consumed (no new data).
    EndOfFile { source_id: Option<SourceId> },
    /// Source has disappeared (file deleted, TCP disconnect).
    /// FramedInput should drop the remainder for this source.
    SourceGone { source_id: SourceId },
}
```

**Why Option<SourceId>**: UDP datagrams and generator inputs have no
meaningful per-source identity. Making it `Option` avoids forcing every
input to fabricate a SourceId. FramedInput falls back to a sentinel
`SourceId(0)` for `None`, which gives the same single-remainder behavior
as today.

### Plumbing: FileTailer to FramedInput

`TailEvent::Data` already carries `path: PathBuf`. `FileInput::poll()`
currently discards the path when converting to `InputEvent::Data`. The fix:

```rust
// FileInput::poll() — convert TailEvent to InputEvent with source_id
TailEvent::Data { path, bytes } => {
    let source_id = self.tailer.source_id_for_path(&path);
    events.push(InputEvent::Data { bytes, source_id: Some(source_id) });
}
TailEvent::Rotated { path } => {
    let source_id = self.tailer.source_id_for_path(&path);
    events.push(InputEvent::Rotated { source_id: Some(source_id) });
}
```

`FileTailer::source_id_for_path` looks up the `TailedFile` for the path
and returns `SourceId(identity.fingerprint)` (same as `file_offsets()`).

---

## 2. Per-Source Remainder in FramedInput

### Data Structures

```rust
/// Per-source framing state.
struct SourceFrameState {
    /// Partial line from the last poll (bytes after the last newline).
    remainder: Vec<u8>,
    /// Byte offset at the START of the current remainder.
    /// This is the file offset at which the remainder bytes begin.
    /// `consumed_offset = remainder_start_offset` (the last complete-line boundary).
    remainder_start_offset: u64,
    /// Total bytes received from this source since the last checkpoint report.
    /// Used to compute consumed_bytes.
    bytes_received_since_report: u64,
}

pub struct FramedInput {
    inner: Box<dyn InputSource>,
    format: FormatProcessor,
    /// Per-source remainder and framing state. Keyed by SourceId.
    /// Sources without an ID (None) map to a sentinel key SourceId(0).
    sources: HashMap<SourceId, SourceFrameState>,
    out_buf: Vec<u8>,
    spare_buf: Vec<u8>,
    stats: Arc<ComponentStats>,
}
```

### Constants and Memory Bounds

```rust
/// Maximum remainder per source (2 MiB, same as today).
const MAX_REMAINDER_BYTES: usize = 2 * 1024 * 1024;

/// Maximum total remainder across all sources (32 MiB).
/// If exceeded, evict the oldest/largest sources.
const MAX_TOTAL_REMAINDER_BYTES: usize = 32 * 1024 * 1024;

/// Maximum number of tracked sources. Beyond this, new sources reuse
/// the sentinel key (falling back to shared-remainder behavior).
const MAX_TRACKED_SOURCES: usize = 4096;
```

### Poll Logic (Pseudocode)

```rust
fn poll(&mut self) -> io::Result<Vec<InputEvent>> {
    let raw_events = self.inner.poll()?;
    let mut result_events = Vec::new();

    for event in raw_events {
        match event {
            InputEvent::Data { bytes, source_id } => {
                let key = source_id.unwrap_or(SourceId(0));
                let state = self.sources
                    .entry(key)
                    .or_insert_with(SourceFrameState::new);

                self.stats.inc_bytes(bytes.len() as u64);

                // Prepend remainder from last poll.
                let mut chunk = std::mem::take(&mut state.remainder);
                chunk.extend_from_slice(&bytes);

                match memchr::memrchr(b'\n', &chunk) {
                    Some(pos) => {
                        if pos + 1 < chunk.len() {
                            state.remainder = chunk.split_off(pos + 1);
                            if state.remainder.len() > MAX_REMAINDER_BYTES {
                                self.stats.inc_parse_errors(1);
                                state.remainder.clear();
                                self.format.reset();
                            }
                            chunk.truncate(pos + 1);
                        }
                        // else: all data ends with newline, remainder stays empty
                    }
                    None => {
                        state.remainder = chunk;
                        if state.remainder.len() > MAX_REMAINDER_BYTES {
                            self.stats.inc_parse_errors(1);
                            state.remainder.clear();
                            self.format.reset();
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
                    let data = std::mem::take(&mut self.out_buf);
                    std::mem::swap(&mut self.out_buf, &mut self.spare_buf);
                    result_events.push(InputEvent::Data {
                        bytes: data,
                        source_id,
                    });
                }
            }

            InputEvent::Rotated { source_id } => {
                if let Some(key) = source_id {
                    if let Some(state) = self.sources.get_mut(&key) {
                        state.remainder.clear();
                    }
                }
                self.format.reset();
                result_events.push(InputEvent::Rotated { source_id });
            }

            InputEvent::Truncated { source_id } => {
                if let Some(key) = source_id {
                    if let Some(state) = self.sources.get_mut(&key) {
                        state.remainder.clear();
                    }
                }
                self.format.reset();
                result_events.push(InputEvent::Truncated { source_id });
            }

            InputEvent::EndOfFile { source_id } => {
                let key = source_id.unwrap_or(SourceId(0));
                if let Some(state) = self.sources.get_mut(&key) {
                    if !state.remainder.is_empty() {
                        state.remainder.push(b'\n');
                        let chunk = std::mem::take(&mut state.remainder);
                        self.out_buf.clear();
                        self.format.process_lines(&chunk, &mut self.out_buf);
                        self.stats.inc_lines(1);
                        if !self.out_buf.is_empty() {
                            let data = std::mem::take(&mut self.out_buf);
                            std::mem::swap(&mut self.out_buf, &mut self.spare_buf);
                            result_events.push(InputEvent::Data {
                                bytes: data,
                                source_id,
                            });
                        }
                    }
                }
            }

            InputEvent::SourceGone { source_id } => {
                // Drop all state for this source (remainder, etc.)
                self.sources.remove(&source_id);
            }
        }
    }

    // Enforce total remainder cap.
    self.enforce_total_remainder_cap();

    Ok(result_events)
}
```

### Source Cleanup

Sources disappear in three ways:

| Trigger | Who detects it | Event |
|---------|---------------|-------|
| File deleted (nlink=0) | `FileTailer::poll()` | `SourceGone` |
| File rotated | `FileTailer::poll()` | `Rotated` (clears remainder, but keeps entry for new file) |
| TCP disconnect | `TcpInput::poll()` | `SourceGone` |

For file rotation, the SourceId changes (new fingerprint), so the old
entry in `FramedInput.sources` becomes orphaned. Two cleanup strategies:

**Eager**: `FileTailer` emits `SourceGone` for the old SourceId before
`Rotated`. FramedInput removes the entry on `SourceGone`.

**Lazy**: `FramedInput` runs a periodic sweep (every N polls or every M
seconds) and removes entries that haven't received data in K polls. This
is simpler but can leak up to `MAX_TRACKED_SOURCES` entries.

**Recommendation**: Eager for file rotation (FileTailer knows the old
SourceId). Lazy sweep as a safety net for leaked entries. Sweep interval:
every 100 polls, remove entries idle for >60s.

---

## 3. Per-Connection Framing in TcpInput

### Current Problem

`TcpInput::poll()` reads from all clients into one `all_data: Vec<u8>`
and emits a single `InputEvent::Data`. A partial line from client A
merges with bytes from client B.

### Design: Per-Connection Line Buffer

```rust
struct Client {
    stream: TcpStream,
    last_data: Instant,
    bytes_since_newline: usize,
    /// Per-connection line buffer. Bytes accumulate here until a
    /// newline is found, at which point complete lines are emitted.
    line_buf: Vec<u8>,
    /// Stable identity for this connection (hash of peer addr + accept time).
    source_id: SourceId,
}
```

### TcpInput::poll() Changes

```rust
fn poll(&mut self) -> io::Result<Vec<InputEvent>> {
    // ... accept new connections (unchanged) ...

    let mut events = Vec::new();

    for (i, client) in self.clients.iter_mut().enumerate() {
        // Read into client.line_buf instead of shared all_data.
        loop {
            match client.stream.read(&mut self.buf) {
                Ok(0) => { alive[i] = false; break; }
                Ok(n) => {
                    client.line_buf.extend_from_slice(&self.buf[..n]);
                    // ... max-line-length check (unchanged) ...
                }
                Err(e) if e.kind() == WouldBlock => break,
                Err(_) => { alive[i] = false; break; }
            }
        }

        // Extract complete lines from line_buf.
        if let Some(last_nl) = memchr::memrchr(b'\n', &client.line_buf) {
            let complete = client.line_buf[..last_nl + 1].to_vec();
            let remainder = client.line_buf.split_off(last_nl + 1);
            client.line_buf = remainder;
            events.push(InputEvent::Data {
                bytes: complete,
                source_id: Some(client.source_id),
            });
        }
    }

    // On disconnect: flush or discard partial line.
    // Policy: discard — a partial line from a disconnected client is
    // likely incomplete. This matches Fluent Bit behavior.
    for (i, client) in self.clients.iter().enumerate() {
        if !alive[i] && !client.line_buf.is_empty() {
            // Emit SourceGone so FramedInput drops its state for this source.
            events.push(InputEvent::SourceGone {
                source_id: client.source_id,
            });
        }
    }

    // ... retain alive clients (unchanged) ...

    Ok(events)
}
```

### Interaction with FramedInput

Since TcpInput now handles newline framing itself, FramedInput becomes a
**passthrough** for TCP inputs. FramedInput still runs format processing
(CRI parsing, etc.) but its per-source remainder is always empty because
TcpInput only emits complete lines.

No special flag is needed: FramedInput's logic naturally passes through
data that ends with `\n` (remainder is empty after split).

### SourceId for TCP Connections

```rust
impl Client {
    fn new(stream: TcpStream, accept_ordinal: u64) -> Self {
        // Hash peer address + accept ordinal for a stable, unique SourceId.
        // The ordinal prevents collisions from the same peer reconnecting.
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        let hash = xxhash_rust::xxh64::xxh64(
            format!("{peer}:{accept_ordinal}").as_bytes(),
            0,
        );
        Self {
            stream,
            last_data: Instant::now(),
            bytes_since_newline: 0,
            line_buf: Vec::with_capacity(4096),
            source_id: SourceId(hash),
        }
    }
}
```

---

## 4. Checkpoint = Processed Offset (Not Read Offset)

### The Fundamental Problem

Today the checkpoint stores `FileTailer.offset`, which is the byte
position of the file descriptor after the last `read()`. But some of
those bytes may sit in FramedInput's remainder buffer, never having
reached the scanner. On crash:

```
read_offset = 1000  (checkpointed)
remainder holds bytes 950..1000
scanner processed up to byte 950

Crash → restart → seek to 1000 → bytes 950..1000 are lost
```

### Solution: Track consumed_bytes per source

FramedInput knows exactly how many bytes it consumed (emitted as complete
lines) vs. how many it kept in the remainder. The pipeline needs to
checkpoint the consumed boundary, not the read boundary.

### New Method on FramedInput

```rust
impl FramedInput {
    /// Return per-source consumed byte counts since the last call.
    ///
    /// "Consumed" means bytes that were split into complete lines and
    /// emitted as `InputEvent::Data`. Remainder bytes are NOT counted.
    ///
    /// The returned map is drained (counts reset to 0) on each call.
    /// The caller adds these to the base offset from the inner source
    /// to compute the processed offset.
    pub fn consumed_bytes(&mut self) -> HashMap<SourceId, u64> {
        self.sources.iter_mut()
            .filter(|(_, state)| state.consumed_since_report > 0)
            .map(|(sid, state)| {
                let consumed = state.consumed_since_report;
                state.consumed_since_report = 0;
                (*sid, consumed)
            })
            .collect()
    }
}
```

### Tracking consumed_since_report in FramedInput

In the `Data` arm of `poll()`, after splitting on the last newline:

```rust
// `chunk` now contains only complete lines (up to and including the last \n).
// `state.remainder` holds the leftover bytes after the last \n.
//
// consumed_bytes for this poll = total bytes received - remainder bytes
let consumed = bytes.len() as u64 - state.remainder.len() as u64;
// Wait — this is wrong if we also prepended the old remainder.

// Correct accounting:
// bytes_in_chunk = old_remainder.len() + bytes.len()
// new_remainder = chunk.split_off(pos + 1) → captured above
// consumed_from_new_data = bytes.len() - new_remainder_from_new_data
//
// Simpler: track the remainder size delta.
// consumed_new_bytes = bytes.len() - (new_remainder.len() - old_remainder.len())
//                    = bytes.len() - new_remainder.len() + old_remainder.len()
// But we need consumed_from_this_source, not from the old remainder...
//
// Actually, the simplest correct approach:
// consumed_offset = read_offset - current_remainder.len()
// So we don't track consumed_since_report at all.
```

### Revised Approach: Snapshot-Based

Instead of incremental tracking, compute the processed offset at
checkpoint time:

```rust
/// FramedInput::checkpoint_data — override the inner source's offsets
/// by subtracting each source's current remainder size.
fn checkpoint_data(&self) -> Vec<(SourceId, ByteOffset)> {
    let raw_offsets = self.inner.checkpoint_data();
    raw_offsets.into_iter().map(|(sid, ByteOffset(read_offset))| {
        let remainder_len = self.sources
            .get(&sid)
            .map(|s| s.remainder.len() as u64)
            .unwrap_or(0);
        (sid, ByteOffset(read_offset - remainder_len))
    }).collect()
}
```

This is dramatically simpler than incremental accounting. The subtraction
is always correct because:
- `read_offset` = position of the file descriptor = bytes read from disk
- `remainder.len()` = bytes read but not yet emitted as complete lines
- `read_offset - remainder.len()` = bytes that made it through framing

### On Restart: Re-Read the Remainder

```text
Shutdown state:
  read_offset = 1000
  remainder.len() = 50
  processed_offset = 950 (checkpointed)

Restart:
  seek(950)
  First read returns bytes 950..1050 (if file grew)
  FramedInput splits: bytes 950..999 contain old partial line + new data
  Result: zero data loss, at-most one line re-processed
```

The "at-most one line re-processed" is acceptable: the alternative (losing
the line) is worse, and most output sinks are idempotent or have
deduplication.

### Flow Through PipelineMachine

No change to PipelineMachine itself. The checkpoint value `C = u64`
remains a byte offset. The only difference is that the offset value
passed to `create_batch(sid, offset)` is now the **processed** offset
(from `FramedInput::checkpoint_data()`) rather than the raw read offset.

The flow:

```text
1. input_poll_loop calls source.checkpoint_data()
   → FramedInput::checkpoint_data() subtracts remainder from inner offsets
   → returns Vec<(SourceId, ByteOffset)> with processed offsets

2. input_poll_loop sends ChannelMsg::Data { checkpoints: processed_offsets }

3. run_async receives message, calls machine.create_batch(sid, offset)
   → offset is now the processed offset

4. On successful output: machine.apply_ack(receipt) advances committed[sid]
   → committed offset is the processed offset

5. Checkpoint store persists committed offset
   → On restart, seek to committed offset, re-read from there
```

---

## 5. UDP Input: No Changes Needed

UDP datagrams are self-contained messages. Each datagram is either fully
received or lost — there is no partial-line problem across datagrams.

`UdpInput` already appends a `\n` to datagrams that lack one. It produces
`InputEvent::Data { source_id: None }` — FramedInput maps this to the
sentinel `SourceId(0)` and provides the same single-remainder behavior
as today. This is correct because UDP has no persistent connections that
could interleave partial lines.

No checkpoint is meaningful for UDP (push source, no offset).

---

## 6. Implementation Plan

### Phase 1: InputEvent + FramedInput Per-Source Remainder

**Files changed:**
- `crates/logfwd-io/src/input.rs` — Add `source_id` to InputEvent variants
- `crates/logfwd-io/src/framed.rs` — HashMap remainder, SourceGone handling
- `crates/logfwd-io/src/tail.rs` — Add `source_id_for_path()` method
- `crates/logfwd-io/src/udp_input.rs` — Emit `source_id: None`
- `crates/logfwd-io/src/tcp_input.rs` — Emit `source_id: None` (temporary)
- `crates/logfwd/src/pipeline.rs` — Update pattern matches on InputEvent

**Tests:**
- `framed.rs`: Two-source interleaved test (source A partial + source B
  complete = no corruption)
- `framed.rs`: SourceGone removes entry
- `framed.rs`: MAX_TRACKED_SOURCES cap
- `framed.rs`: Total remainder cap
- `framed.rs`: Backward compat (source_id=None uses sentinel)
- `input.rs`: FileInput emits source_id from tailer

**Order:** This is the foundation. Can land independently.

### Phase 2: Per-Connection Framing in TcpInput

**Files changed:**
- `crates/logfwd-io/src/tcp_input.rs` — Per-conn line_buf + SourceId
- `crates/logfwd-io/src/input.rs` — SourceGone variant (if not in Phase 1)

**Tests:**
- `tcp_input.rs`: Two connections interleaved, no corruption
- `tcp_input.rs`: Disconnect flushes/discards partial
- `tcp_input.rs`: SourceGone emitted on disconnect

**Order:** Independent of Phase 1. Can be done in parallel. But benefits
from Phase 1's InputEvent changes, so sequence after Phase 1 for less churn.

### Phase 3: Checkpoint = Processed Offset

**Files changed:**
- `crates/logfwd-io/src/framed.rs` — Override `checkpoint_data()` to
  subtract remainder size
- `crates/logfwd/src/pipeline.rs` — No change (already uses
  `source.checkpoint_data()`)
- `crates/logfwd-io/src/checkpoint.rs` — No change (stores opaque offset)

**Tests:**
- `framed.rs`: `checkpoint_data()` returns read_offset - remainder.len()
- `framed.rs`: After poll with partial line, checkpoint < read_offset
- `framed.rs`: After poll with complete lines, checkpoint == read_offset
- Integration test: write partial line → checkpoint → "restart" (new
  FramedInput at checkpoint offset) → write rest of line → data recovered

**Order:** Depends on Phase 1 (per-source remainder). Small diff.

### Phase 4: FileTailer SourceGone on Deletion/Rotation

**Files changed:**
- `crates/logfwd-io/src/tail.rs` — Emit `TailEvent::SourceGone` with old
  SourceId before removing from `self.files`
- `crates/logfwd-io/src/input.rs` — Convert `TailEvent::SourceGone` to
  `InputEvent::SourceGone`

**Tests:**
- `tail.rs`: File deletion emits SourceGone before removal
- `tail.rs`: File rotation emits SourceGone(old_id) then Rotated
- `framed.rs`: SourceGone removes HashMap entry

**Order:** After Phase 1. Small diff.

### Summary

| Phase | Scope | Risk | Can ship independently? |
|-------|-------|------|------------------------|
| 1 | Per-source remainder | Medium (touches many files) | Yes |
| 2 | TCP per-connection | Low (isolated to tcp_input.rs) | Yes, after Phase 1 |
| 3 | Processed-offset checkpoint | Low (one method override) | Yes, after Phase 1 |
| 4 | SourceGone cleanup | Low (plumbing) | Yes, after Phase 1 |

Total estimated scope: ~400 lines of production code, ~300 lines of tests.

---

## 7. Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| SourceId in InputEvent | `Option<SourceId>` | UDP/generators don't have source identity |
| Sentinel for None | `SourceId(0)` | Fingerprint 0 is already reserved for empty files; overloading is safe because empty files produce no data |
| Remainder per source | `HashMap<SourceId, SourceFrameState>` | Minimal change to FramedInput, correct |
| TCP framing | Per-connection in TcpInput | TcpInput already tracks per-connection state; avoids FramedInput complexity |
| Processed offset | `read_offset - remainder.len()` | Snapshot at checkpoint time, no incremental bookkeeping |
| Disconnect policy (TCP) | Discard partial | Partial from disconnected client is likely truncated; emitting it risks malformed records |
| Total remainder cap | 32 MiB | Prevents OOM with many sources, each with large partials |
| Source cleanup | Eager (SourceGone) + lazy sweep | Eager handles known cases; sweep catches leaks |

---

## 8. Migration Concerns

### Backward Compatibility

- Inputs that don't set `source_id` (UDP, generators, third-party
  InputSource impls) automatically get `None`, which maps to the sentinel
  `SourceId(0)`. Behavior is identical to today.

- The `InputEvent` enum is `#[non_exhaustive]`, so adding `SourceGone`
  and adding fields to existing variants is a non-breaking change for
  external consumers (they already must have a wildcard match arm).

- Adding `source_id: Option<SourceId>` to `InputEvent::Data` changes
  the struct variant's shape. All pattern matches in the codebase (pipeline.rs,
  framed.rs tests) need updating. This is a codebase-wide but mechanical change.

### Checkpoint File Format

The checkpoint file stores `SourceCheckpoint { source_id, path, offset }`.
The offset value changes meaning (read offset -> processed offset) but
the format is unchanged. On upgrade: the first checkpoint after restart
may be slightly behind (remainder was not subtracted in the old version),
causing at most one batch of duplicate lines. Acceptable.

On downgrade: the processed offset is lower than what the old code would
have written. The old code seeks to a position that's behind where it
left off, re-reads some lines. Same acceptable duplicate behavior.

### FormatProcessor State

`FormatProcessor` (CRI aggregator, auto-detect) currently has shared
state across all sources. A partial CRI multi-line record from source A
could leak into source B's output.

Phase 1 does NOT fix this — it only fixes the remainder buffer. Fixing
per-source format state requires either:
- Per-source FormatProcessor (expensive: CRI aggregator allocates)
- Keying the CRI aggregator's partial-record buffer by SourceId

This is a follow-up issue, not a blocker for Phase 1. The CRI aggregator
corruption is rarer than remainder corruption because CRI partial records
(`P` flag) are uncommon in multi-file inputs.
