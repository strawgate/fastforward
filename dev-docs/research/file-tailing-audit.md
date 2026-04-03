# File Tailing Audit — Comparison with Industry Collectors

Date: 2026-04-02
Context: End-to-end audit of logfwd's file reading path, compared with
Vector, OTel Collector filelog, and Fluent Bit in_tail.

## Critical Findings

### 1. Shared remainder buffer across files (#797)

**Every other collector has per-file buffers.** Vector uses per-FileWatcher
`BytesMut`, OTel has per-file Reader with own decoder/split function,
Fluent Bit has per-file `buf_data` heap allocation. logfwd uses a single
`FramedInput` remainder buffer shared across all files in a glob input.

**Impact**: If file A writes a partial line (no `\n`) and file B's data
arrives next, file A's bytes are prepended to file B's data, producing
a corrupted merged line.

**Fix**: Per-file remainder tracking, keyed by SourceId or path.

### 2. TailEvent::Truncated never emitted (#796)

**Fluent Bit explicitly clears the per-file buffer on truncation** (sets
`buf_len = 0`). OTel Collector has configurable truncation behavior
(ignore/read_whole/read_new). logfwd detects truncation in `read_new_data`
but never emits `TailEvent::Truncated`, so `FramedInput` never clears
its remainder.

**Impact**: On copytruncate rotation, stale partial-line bytes from the
old file content get prepended to new content.

**Fix**: Emit `TailEvent::Truncated` when truncation detected.

### 3. Unbounded read in read_new_data

**Vector limits reads to 2 KiB per file per poll cycle.** Fluent Bit
limits to one chunk (32 KB default) with configurable batch limits
(50 MB). OTel Collector reads to EOF but per-file in goroutines.
logfwd reads ALL available data from a file into an unbounded `Vec<u8>`.

**Impact**: A file that grows by 10 GB between polls causes a 10 GB
allocation in a single `read_new_data` call.

**Fix**: Cap `read_new_data` to a configurable max (e.g., 16 MiB).

### 4. Glob dedup by PathBuf string (#799)

**Vector deduplicates by fingerprint.** Fluent Bit deduplicates by
`dev_id:inode` hash. OTel Collector deduplicates by fingerprint (raw
bytes). logfwd deduplicates by `PathBuf` string comparison.

**Impact**: Symlinks or different relative/absolute paths to the same
file bypass dedup, causing duplicate reads.

**Fix**: Deduplicate by `(device, inode)` pair.

### 5. Fingerprint collision causes checkpoint misattribution (#798)

**Fluent Bit uses dev+inode (no collision possible).** Vector uses CRC-64
with mtime tiebreaker. OTel uses raw bytes (no hash, exact comparison).
logfwd uses xxh64 hash only — collision = silent misattribution.

**Fix**: Include `(device, inode)` in SourceId hash.

## Medium Findings

### 6. No partial line flush timer

OTel Collector has a 500ms per-file timer that flushes partial data if
no new data arrives. This is persisted in checkpoints. Vector and Fluent
Bit have no timer (partial data sits until file is deleted/rotated).
logfwd flushes on EndOfFile event, which requires a full poll cycle with
no new data.

The OTel approach is the gold standard — guarantees partial lines are
emitted within 500ms of the last write.

### 7. Partial line lost on shutdown

Vector handles this elegantly: the `file_position` is not advanced past
the BufReader's internal buffer, so partial data is re-read on restart.
OTel Collector flushes with a 5s timeout. Fluent Bit and logfwd both
discard partial lines on shutdown.

### 8. No per-file read fairness

Vector reads max 2 KiB per file per cycle for fairness. Fluent Bit
reads one chunk per file. logfwd reads ALL available data from each
file before moving to the next.

**Impact**: One chatty file can starve quiet files of processing time.

## Comparison Table

| Aspect | Vector | OTel Collector | Fluent Bit | logfwd |
|--------|--------|---------------|------------|--------|
| Per-file buffer | BytesMut per watcher | Per-file Reader | Per-file buf_data | **Shared** |
| Partial flush timer | No | 500ms per-file | No | No |
| Truncation handling | No explicit detection | Configurable | Detect + clear buffer | **Detect, no event** |
| File identity | CRC-64 first line / dev+inode | Raw first 1000 bytes | dev+inode | xxh64 first 1024 bytes |
| Read budget/file | 2 KiB/cycle | Full file (goroutine) | 32 KB chunk | **Unbounded** |
| Glob dedup | By fingerprint | By fingerprint | By dev:inode | **By path string** |
| Checkpoint | JSON, atomic rename | Protobuf, per-poll | SQLite WAL, per-chunk | JSON, 5s interval |
| Max line | 100 KiB | 1 MiB | 32 KB | 2 MiB |
| Backpressure | Channel cap 2 | Blocking emit | Pause collectors | Channel cap 16 |

## Priority Fixes

1. Per-file remainder buffers (#797) — CRITICAL
2. Emit TailEvent::Truncated (#796) — CRITICAL, one-line fix
3. Bound read_new_data — HIGH, new issue needed
4. Glob dedup by inode (#799) — HIGH
5. Fingerprint collision (#798) — HIGH
6. Per-file read fairness budget — MEDIUM
7. Partial line flush timer — MEDIUM (OTel pattern)

## References

- Vector: `lib/file-source/src/file_watcher/mod.rs`, `lib/file-source-common/src/buffer.rs`
- OTel Collector: `pkg/stanza/fileconsumer/`, `pkg/stanza/flush/flush.go`
- Fluent Bit: `plugins/in_tail/tail_file.c`, `plugins/in_tail/tail_fs_inotify.c`
- logfwd: `crates/logfwd-io/src/tail.rs`, `crates/logfwd-io/src/framed.rs`
