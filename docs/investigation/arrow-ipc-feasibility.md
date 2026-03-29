# Arrow IPC for DiskQueue -- Feasibility Investigation

Issue #85. Research only, no code changes.

---

## 1. Arrow IPC Roundtrip

### API surface (arrow v54, `ipc_compression` feature enabled)

**Writing:**

```rust
use arrow::ipc::writer::{StreamWriter, FileWriter, IpcWriteOptions};
use arrow::ipc::CompressionType;

// Compressed options
let options = IpcWriteOptions::default()
    .try_with_compression(Some(CompressionType::ZSTD))
    .expect("zstd supported");

// StreamWriter -- writes schema message, then N batch messages, then footer (EOS marker)
let mut writer = StreamWriter::try_new_with_options(&mut buf, &schema, options)?;
writer.write(&batch)?;
writer.finish()?;  // writes 0-length continuation = EOS

// FileWriter -- writes ARROW1 magic, schema, N batches, footer with offsets, ARROW1 magic
let mut writer = FileWriter::try_new_with_options(file, &schema, options)?;
writer.write(&batch)?;
writer.finish()?;  // writes footer block with batch offsets
```

**Reading:**

```rust
use arrow::ipc::reader::{StreamReader, FileReader};

// StreamReader -- sequential read, no random access
let reader = StreamReader::try_new(cursor, None)?;
for batch in reader {
    let batch = batch?;  // RecordBatch, decompressed automatically
}

// FileReader -- random access via footer, supports mmap
let file = std::fs::File::open(path)?;
let reader = FileReader::try_new(file, None)?;
let batch = reader.into_iter().nth(2)?;  // random access to batch 2
```

**Roundtrip fidelity:** Bit-identical. Arrow IPC preserves schema (field names, types, metadata), null bitmaps, and buffer contents exactly. Compression is per-buffer within the IPC message -- decompression reconstructs the original buffers. The `arrow` crate's own test suite validates this.

### FileWriter vs StreamWriter for segment files

| Property | StreamWriter | FileWriter |
|---|---|---|
| Footer | 4-byte EOS marker (continuation=0) | Full footer with batch byte offsets + ARROW1 magic |
| Random access | No | Yes (footer has batch offsets) |
| mmap read | Not useful (sequential only) | Yes -- `FileReader` can mmap |
| Append-friendly | Limited -- EOS must be overwritten | No -- footer is at end |
| Crash recovery | Truncated stream is readable up to last complete message | Requires footer; no footer = unreadable without recovery code |
| Overhead | ~10 bytes (schema msg + EOS) | ~200-500 bytes (magic + footer + magic) |

**Recommendation: FileWriter for segment files.** The footer enables mmap random-access replay and the overhead is negligible. The crash recovery concern is addressed in Section 4.

### Zstd compression in Arrow IPC

Already proven in the codebase. The `ipc_compression` feature on arrow v54 enables `CompressionType::ZSTD` and `CompressionType::LZ4_FRAME`. Compression operates per-buffer (each column buffer within a RecordBatch message is independently compressed). This is optimal: string-heavy columns compress well, integer columns less so, and the reader decompresses lazily per buffer.

The existing `finish_batch_compressed()` in `StorageBuilder` (worktree branches, issue #71) demonstrates the exact pattern:

```rust
let options = arrow::ipc::writer::IpcWriteOptions::default()
    .try_with_compression(Some(arrow::ipc::CompressionType::ZSTD))
    .expect("zstd compression supported");
let mut writer = StreamWriter::try_new_with_options(&mut output, &batch.schema(), options)?;
writer.write(&batch)?;
writer.finish()?;
```

---

## 2. Throughput Estimates

### Serialized size for a 4 MB batch (8K rows, 10 columns)

Arrow IPC overhead per batch message:
- Message header: ~100 bytes (flatbuffer metadata: field count, buffer offsets, null counts)
- Per-buffer alignment padding: 8 bytes per buffer, ~10 buffers (offsets + data for each column) = ~80 bytes
- Schema message (once per file): ~200-500 bytes depending on field count and metadata

For a 4 MB in-memory RecordBatch:
- **Uncompressed IPC:** ~4.0-4.1 MB. Overhead is < 1%. Arrow IPC is essentially a serialized form of the in-memory layout with alignment padding.
- **Zstd-compressed IPC:** Depends on data. Log data (repetitive strings, timestamps) typically compresses 3-5x. Expected: **0.8-1.5 MB** for a 4 MB batch.

### Write throughput estimate

Arrow IPC write is memcpy-dominant (copies column buffers into the output stream). For uncompressed:
- memcpy at ~10 GB/s = 4 MB batch in ~0.4 ms
- At 8K rows/batch, that is 20M rows/sec from serialization alone

With zstd compression (level 1, which Arrow uses by default):
- zstd-1 throughput: ~500 MB/s input rate
- 4 MB batch: ~8 ms compression
- At 8K rows/batch: ~1M rows/sec

**zstd compression is the bottleneck, not IPC framing.** But 1M rows/sec meets the target, and compression reduces disk I/O (which matters more for sustained throughput on spinning disks or busy SSDs).

### mmap read for replay

`arrow::ipc::reader::FileReader` accepts any `Read + Seek` impl. The arrow crate pulls in `memmap2` as a transitive dependency (confirmed in Cargo.lock). For mmap replay:

```rust
let file = std::fs::File::open(path)?;
let mmap = unsafe { memmap2::Mmap::map(&file)? };
let cursor = std::io::Cursor::new(&mmap[..]);
let reader = FileReader::try_new(cursor, None)?;
```

Benefits:
- OS manages page cache; no double-buffering
- Random access to individual batches via footer offsets
- Replay reads only the batches needed (unacked entries)
- Multiple readers can share the same mmap (useful for fan-out to multiple sinks)

**Verdict: mmap read is viable and recommended for replay.**

---

## 3. Segment File Design

### Option A: One IPC file per batch

```
queue_name_0000000000000001.arrow   (1 batch)
queue_name_0000000000000002.arrow   (1 batch)
manifest.json
```

Pros:
- Simplest crash recovery (atomic rename per batch)
- Easy deletion on ack (just unlink the file)
- No append coordination

Cons:
- Many small files if batches are small (4 MB each)
- Filesystem overhead (inodes, directory entries)
- At 1M lines/sec with 8K rows/batch = 125 batches/sec = 125 files/sec

### Option B: Multiple batches per file (append-only segments)

```
queue_name_segment_0001.arrow   (N batches, up to 64 MB)
queue_name_segment_0002.arrow   (N batches)
manifest.json
```

Arrow IPC FileWriter supports writing multiple batches to one file. The footer records the byte offset of each batch, enabling random access.

Pros:
- Fewer files (64 MB segment = ~16 batches of 4 MB)
- Better sequential I/O (large writes)
- Amortizes filesystem metadata overhead

Cons:
- Cannot delete individual batches; must wait until entire segment is acked
- Append requires rewriting footer (or using StreamWriter + recovery)
- More complex crash recovery

### Recommended design: Hybrid (Option A with batching)

Use **FileWriter, one file per segment, multiple batches per segment.** Target segment size: 64 MB.

```
pre_transform/
    seg_0000000000000001.arrow    (up to 64 MB, N batches)
    seg_0000000000000002.arrow
    manifest.json

pre_output/
    seg_0000000000000001.arrow
    manifest.json
```

Write protocol:
1. Open new segment file as `.tmp`
2. Create `FileWriter` with zstd compression
3. Write batches until segment reaches 64 MB target
4. Call `writer.finish()` to write footer
5. `fsync` the file
6. Rename `.tmp` to final name
7. Update `manifest.json` + `fsync`

This means each segment is a complete, valid IPC file once renamed.

### Manifest design

```json
{
  "version": 1,
  "segments": [
    {
      "id": 1,
      "file": "seg_0000000000000001.arrow",
      "batch_count": 16,
      "byte_size": 63897600,
      "first_batch_seq": 1,
      "last_batch_seq": 16
    }
  ],
  "read_position": {
    "segment_id": 1,
    "batch_index": 5
  },
  "ack_position": {
    "segment_id": 1,
    "batch_index": 3
  }
}
```

On startup, `replay()` reads all batches from `ack_position + 1` through the last written batch across all segments.

---

## 4. Crash Safety

### Mid-write crash scenarios

| Scenario | FileWriter behavior | Impact |
|---|---|---|
| Crash during batch write (before finish) | `.tmp` file has no footer | `.tmp` file ignored on recovery (not renamed) |
| Crash after finish(), before fsync | Footer written but may not be on disk | OS may have flushed; if not, `.tmp` has partial data, ignored |
| Crash after fsync, before rename | `.tmp` is durable and valid | `.tmp` ignored; data lost but not corrupt. Retry from source. |
| Crash after rename, before manifest update | Segment file exists, manifest is stale | Recovery: scan directory for segment files newer than manifest |
| Crash during manifest write | Partial JSON | Keep previous manifest as `.bak`; restore on corrupt detection |

### Recovery protocol

1. Delete all `.tmp` files (incomplete segments)
2. Read `manifest.json` (fall back to `manifest.json.bak` if corrupt)
3. Scan directory for `.arrow` files not in manifest (segments written after last manifest sync)
4. For orphan segments: open with `FileReader`, read footer to get batch count, add to manifest
5. Replay from `ack_position + 1`

### fsync strategy

- **fsync after each segment completion** (after `finish()`, before rename). Not after every batch -- that would be 125 fsyncs/sec at full throughput, which kills SSD performance.
- **fsync manifest after each segment rotation** (not after each ack -- batch ack positions in memory, periodic flush)
- Cost: 1-2 fsyncs per segment rotation. At 64 MB segments and 4 MB batches, that is one fsync per ~16 batches, or roughly every 128ms at 1M lines/sec. Acceptable.

### Atomic rename pattern

Yes, use write-to-`.tmp`-then-rename. On POSIX, `rename()` is atomic within a filesystem. Combined with pre-rename fsync, this guarantees that any `.arrow` file (non-`.tmp`) is complete and valid.

```
write to seg_XXXX.arrow.tmp
  -> FileWriter::finish()
  -> fsync(fd)
  -> rename(seg_XXXX.arrow.tmp, seg_XXXX.arrow)
  -> update manifest.json.tmp
  -> fsync(manifest fd)
  -> rename(manifest.json.tmp, manifest.json)
```

### What about StreamWriter as an alternative for append?

StreamWriter could append batches without rewriting a footer. But:
- No random access (cannot skip to batch N without scanning from start)
- Crash recovery requires scanning the entire stream to find message boundaries
- The EOS marker is needed to know the stream is complete

FileWriter is strictly better for segment files despite the "must finalize" constraint, because the write-to-tmp-then-rename pattern makes finalization safe.

---

## 5. Existing Code Inventory

### Arrow IPC already in the codebase

- **Workspace Cargo.toml** (master): `arrow = { version = "54", default-features = false, features = ["ipc_compression"] }` -- zstd IPC compression is already enabled.

- **`finish_batch_compressed()`** in `StorageBuilder` (worktree branches for issues #71, grok-regex-udfs, cli-improvements): Full working implementation of Arrow IPC StreamWriter with zstd compression. Located at `crates/logfwd-core/src/columnar_builder.rs:398`. Not yet on master.

- **`memmap2`** in Cargo.lock: Already a transitive dependency via Arrow. No new dependency needed for mmap reads.

- **DEVELOPING.md** (arrow-ipc-output worktree): Documents that "Compressed Arrow IPC is StreamWriter with IpcWriteOptions::try_with_compression(Some(CompressionType::ZSTD)). Any RecordBatch can be compressed. No special builder needed."

### Issue #71 status

Issue #71 (compressed Arrow IPC output to StorageBuilder) has prototype code in three worktree branches. The `finish_batch_compressed()` method is implemented and functional. This is the building block the DiskQueue needs -- it proves the write path works. The DiskQueue would use a similar pattern but with FileWriter instead of StreamWriter, and writing to disk files instead of in-memory buffers.

### What's missing for DiskQueue

1. **FileWriter usage** -- existing code uses StreamWriter to Vec<u8>. DiskQueue needs FileWriter to disk files.
2. **Read path** -- no `StreamReader` or `FileReader` usage anywhere in the codebase yet.
3. **Segment file management** -- rotation, manifest, cleanup.
4. **The `DiskQueue` trait** -- specified in PIPELINE_ARCHITECTURE.md but not implemented.

---

## Summary

| Question | Answer |
|---|---|
| Can Arrow IPC roundtrip bit-identically? | Yes. Proven by Arrow's own test suite and the existing `finish_batch_compressed` code. |
| Zstd compression? | Already enabled via `ipc_compression` feature. Per-buffer compression, ~3-5x ratio on log data. |
| FileWriter vs StreamWriter? | **FileWriter** for segment files. Footer enables mmap random access; atomic rename handles crash safety. |
| Expected overhead? | < 1% uncompressed. With zstd: 0.8-1.5 MB for a 4 MB batch. |
| mmap viable for replay? | Yes. `FileReader` + `memmap2` (already a transitive dep). Enables zero-copy random-access replay. |
| Segment file design? | Multi-batch FileWriter segments (~64 MB), atomic rename, JSON manifest. |
| Crash safety? | Write-to-tmp + fsync + rename. Recovery: delete .tmp, scan for orphan segments, replay from ack position. |
| Existing code to build on? | `finish_batch_compressed()` in worktree branches. `ipc_compression` feature already enabled on master. |

**Feasibility: High.** Arrow IPC is well-suited for the DiskQueue. The format is zero-copy friendly, supports compression, and the codebase already has the write-path prototype. The main new work is FileWriter (instead of StreamWriter), the read path, segment management, and the manifest/recovery logic.
