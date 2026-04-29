//! Pipelined sink: separates serialization (CPU) from transport (I/O).
//!
//! The pipeline uses a dedicated OS thread for I/O and a small buffer pool to
//! keep blocking file writes off the caller's serialization path without
//! allocation churn.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────┐     sync_channel      ┌──────────────────┐
//! │  Caller thread   │ ──── filled buf ────▶ │  Writer thread    │
//! │  (tokio worker)  │ ◀─── empty buf ───── │  (OS thread)      │
//! │                  │                       │                   │
//! │  serialize(batch)│                       │  writer.write(buf)│
//! └─────────────────┘                       └──────────────────┘
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! let sink = PipelinedSink::new(serializer, writer, stats, PipelineConfig::default())?;
//! // sink implements Sink trait — use it in the worker pool as normal
//! ```

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::{self as std_mpsc, SyncSender};
use std::thread::{self, JoinHandle};

use arrow::record_batch::RecordBatch;
use ffwd_types::diagnostics::ComponentStats;

use crate::BatchMetadata;
use crate::sink::{SendResult, Sink};

// ---------------------------------------------------------------------------
// BatchSerializer trait
// ---------------------------------------------------------------------------

/// Pure CPU serialization: converts a `RecordBatch` into bytes.
///
/// Implementors MUST NOT perform any I/O. This trait is intentionally
/// synchronous to enforce that constraint at the type level.
pub trait BatchSerializer: Send {
    /// Serialize `batch` into `buf`, appending bytes.
    ///
    /// Returns the number of logical rows written (may differ from
    /// `batch.num_rows()` if some rows are filtered/skipped).
    fn serialize(
        &mut self,
        batch: &RecordBatch,
        metadata: &BatchMetadata,
        buf: &mut Vec<u8>,
    ) -> io::Result<u64>;

    /// Human-readable name for logging and diagnostics.
    fn name(&self) -> &str;
}

// ---------------------------------------------------------------------------
// BatchWriter trait
// ---------------------------------------------------------------------------

/// I/O transport: takes serialized bytes and delivers them to a destination.
///
/// Runs on a dedicated OS thread. Implementations own the file descriptor,
/// socket, or HTTP client and perform blocking I/O.
pub trait BatchWriter: Send + 'static {
    /// Write the serialized payload to the destination.
    fn write(&mut self, buf: &[u8]) -> io::Result<()>;

    /// Flush internal buffers (if any) to the OS.
    fn flush(&mut self) -> io::Result<()>;

    /// Graceful shutdown: flush + fsync / close connection.
    fn shutdown(&mut self) -> io::Result<()>;
}

// ---------------------------------------------------------------------------
// PipelineConfig
// ---------------------------------------------------------------------------

/// Configuration for the pipelined sink.
pub struct PipelineConfig {
    /// Number of buffers in the pool. These are cycled between the serializer
    /// and writer threads for zero-allocation steady-state operation.
    /// Default: 3 (triple-buffered: one being serialized, one in flight, one
    /// being written).
    pub num_buffers: usize,

    /// Initial capacity for each buffer in bytes.
    /// Default: 2 MB.
    pub buf_capacity: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            num_buffers: 3,
            buf_capacity: 2 * 1024 * 1024,
        }
    }
}

// ---------------------------------------------------------------------------
// Writer thread messages
// ---------------------------------------------------------------------------

enum WriterMsg {
    /// A filled buffer ready to be written.
    Data {
        buf: Vec<u8>,
        ack: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    /// Flush request with a one-shot ack channel.
    Flush(SyncSender<io::Result<()>>),
    /// Shutdown request — writer should flush, sync, then exit.
    Shutdown(SyncSender<io::Result<()>>),
}

// ---------------------------------------------------------------------------
// PipelinedSink
// ---------------------------------------------------------------------------

/// A `Sink` implementation that pipelines serialization and I/O on separate
/// threads for maximum throughput.
pub struct PipelinedSink<S: BatchSerializer> {
    serializer: S,
    stats: Arc<ComponentStats>,

    /// Channel to send filled buffers (and control messages) to the writer.
    filled_tx: Option<SyncSender<WriterMsg>>,

    /// Channel to receive empty (recycled) buffers back from the writer.
    empty_rx: std_mpsc::Receiver<Vec<u8>>,

    /// Caller-owned spare buffer for serialization failures.
    spare_buf: Option<Vec<u8>>,

    /// Handle to the writer thread (for join on shutdown).
    writer_handle: Option<JoinHandle<()>>,
}

impl<S: BatchSerializer> PipelinedSink<S> {
    /// Create a new pipelined sink.
    ///
    /// Spawns a dedicated OS thread for the writer immediately.
    /// Returns an error if the thread cannot be spawned or config is invalid.
    pub fn new(
        serializer: S,
        writer: impl BatchWriter,
        stats: Arc<ComponentStats>,
        config: PipelineConfig,
    ) -> io::Result<Self> {
        if config.num_buffers == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PipelineConfig::num_buffers must be >= 1",
            ));
        }

        let (filled_tx, filled_rx) = std_mpsc::sync_channel::<WriterMsg>(config.num_buffers);
        let (empty_tx, empty_rx) = std_mpsc::sync_channel::<Vec<u8>>(config.num_buffers);

        // Seed the empty buffer pool.
        for _ in 0..config.num_buffers {
            let _ = empty_tx.send(Vec::with_capacity(config.buf_capacity));
        }

        let writer_handle = thread::Builder::new()
            .name(format!("ffwd-writer-{}", serializer.name()))
            .spawn(move || {
                writer_thread_loop(writer, filled_rx, empty_tx);
            })
            .map_err(io::Error::other)?;

        Ok(Self {
            serializer,
            stats,
            filled_tx: Some(filled_tx),
            empty_rx,
            spare_buf: None,
            writer_handle: Some(writer_handle),
        })
    }

    /// Try to get an empty buffer from the pool without blocking the async
    /// worker thread.
    fn acquire_buffer(&mut self) -> io::Result<Vec<u8>> {
        if let Some(buf) = self.spare_buf.take() {
            return Ok(buf);
        }

        match self.empty_rx.try_recv() {
            Ok(buf) => Ok(buf),
            Err(std_mpsc::TryRecvError::Empty) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "pipelined writer has no available buffer",
            )),
            Err(std_mpsc::TryRecvError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "writer thread exited unexpectedly",
            )),
        }
    }

    /// Clone the writer-thread sender before awaiting an acknowledgement.
    fn filled_sender(&self) -> io::Result<SyncSender<WriterMsg>> {
        self.filled_tx
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "sink already shut down"))
    }

    /// Send a filled buffer to the writer thread and wait for it to be written.
    async fn write_filled(tx: SyncSender<WriterMsg>, buf: Vec<u8>) -> io::Result<()> {
        let (ack, ack_rx) = tokio::sync::oneshot::channel();
        tx.send(WriterMsg::Data { buf, ack }).map_err(|_send| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "writer thread exited unexpectedly",
            )
        })?;
        ack_rx.await.map_err(|_recv| {
            io::Error::new(io::ErrorKind::BrokenPipe, "writer thread exited before ack")
        })?
    }

    /// Send a control message and wait for the ack.
    fn send_control<F>(&self, make_msg: F) -> io::Result<()>
    where
        F: FnOnce(SyncSender<io::Result<()>>) -> WriterMsg,
    {
        let (ack_tx, ack_rx) = std_mpsc::sync_channel(1);
        if let Some(ref tx) = self.filled_tx {
            tx.send(make_msg(ack_tx)).map_err(|_send| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "writer thread exited unexpectedly",
                )
            })?;
            ack_rx.recv().map_err(|_recv| {
                io::Error::new(io::ErrorKind::BrokenPipe, "writer thread exited before ack")
            })?
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "sink already shut down",
            ))
        }
    }
}

impl<S: BatchSerializer> Sink for PipelinedSink<S> {
    fn send_batch<'a>(
        &'a mut self,
        batch: &'a RecordBatch,
        metadata: &'a BatchMetadata,
    ) -> Pin<Box<dyn Future<Output = SendResult> + Send + 'a>> {
        Box::pin(async move {
            // Acquire an empty buffer from the pool without blocking the Tokio
            // worker thread. If all buffers are still owned by in-flight
            // timed-out writes, surface WouldBlock so the worker pool applies
            // its normal transient-error backoff.
            let mut buf = match self.acquire_buffer() {
                Ok(b) => b,
                Err(e) => return SendResult::from_io_error(e),
            };
            buf.clear();

            // Serialize on the caller thread (CPU work).
            let rows = match self.serializer.serialize(batch, metadata, &mut buf) {
                Ok(n) => n,
                Err(e) => {
                    // Keep the caller-owned buffer available without depending
                    // on the writer thread, which may already be unhealthy.
                    buf.clear();
                    self.spare_buf = Some(buf);
                    return SendResult::from_io_error(e);
                }
            };

            let bytes = buf.len() as u64;

            // Hand off the filled buffer and wait for the writer to report the
            // OS write result before acknowledging delivery to the worker pool.
            let filled_tx = match self.filled_sender() {
                Ok(tx) => tx,
                Err(e) => return SendResult::from_io_error(e),
            };
            if let Err(e) = Self::write_filled(filled_tx, buf).await {
                return SendResult::from_io_error(e);
            }

            self.stats.inc_lines(rows);
            self.stats.inc_bytes(bytes);
            SendResult::Ok
        })
    }

    fn flush(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async move { self.send_control(WriterMsg::Flush) })
    }

    fn name(&self) -> &str {
        self.serializer.name()
    }

    fn shutdown(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async move {
            // Send shutdown message and wait for ack.
            let result = self.send_control(WriterMsg::Shutdown);

            // Drop the sender so the writer thread exits its loop.
            self.filled_tx.take();

            // Join the writer thread.
            if let Some(handle) = self.writer_handle.take() {
                let _ = handle.join();
            }

            result
        })
    }
}

impl<S: BatchSerializer> Drop for PipelinedSink<S> {
    fn drop(&mut self) {
        // Best-effort shutdown if not already done.
        self.filled_tx.take();
        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Writer thread loop
// ---------------------------------------------------------------------------

fn writer_thread_loop(
    mut writer: impl BatchWriter,
    filled_rx: std_mpsc::Receiver<WriterMsg>,
    empty_tx: SyncSender<Vec<u8>>,
) {
    while let Ok(msg) = filled_rx.recv() {
        match msg {
            WriterMsg::Data { buf, ack } => {
                let result = writer.write(&buf);
                if let Err(e) = &result {
                    tracing::error!(error = %e, "pipelined writer: write failed");
                }
                // Recycle the buffer back to the pool.
                let mut recycled = buf;
                recycled.clear();
                // Preserve the fixed-size pool. If send fails, the sink is
                // being dropped, so letting the buffer go is fine.
                let _ = empty_tx.send(recycled);
                let _ = ack.send(result);
            }
            WriterMsg::Flush(ack) => {
                let result = writer.flush();
                let _ = ack.send(result);
            }
            WriterMsg::Shutdown(ack) => {
                let result = writer.shutdown();
                let _ = ack.send(result);
                return;
            }
        }
    }
    // Channel closed without Shutdown — best-effort shutdown.
    let _ = writer.shutdown();
}

// ---------------------------------------------------------------------------
// FileWriter
// ---------------------------------------------------------------------------

/// Pre-allocation chunk size: 64 MB.
///
/// On Linux/macOS, when the file is opened, we ask the OS to reserve this much
/// space up front. This reduces per-write metadata updates on filesystems such
/// as ext4, XFS, and APFS during sequential extending writes.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const PREALLOC_BYTES: u64 = 64 * 1024 * 1024;

/// A [`BatchWriter`] that writes to a file using blocking std::fs I/O.
///
/// Applies platform-specific optimizations on construction:
///
/// | Platform | Optimization |
/// |----------|-------------|
/// | Linux | `fallocate` pre-allocation, `POSIX_FADV_DONTNEED` after writes via `rustix` |
/// | macOS | `F_PREALLOCATE`, `F_NOCACHE` (bypass UBC for written pages) |
/// | Windows | (standard `write_all` — no extra APIs yet) |
pub struct FileWriter {
    file: std::fs::File,
    /// Cumulative bytes written — used for `FADV_DONTNEED` offset tracking.
    #[cfg(target_os = "linux")]
    bytes_written: u64,
}

impl FileWriter {
    /// Create a new file writer, applying platform-specific hints.
    pub fn new(file: std::fs::File) -> Self {
        Self::apply_platform_hints(&file);

        // Initialize bytes_written to the current file position so that
        // FADV_DONTNEED offsets are correct when appending to existing files.
        #[cfg(target_os = "linux")]
        let bytes_written = file.metadata().map_or_else(|_| 0, |m| m.len());

        Self {
            file,
            #[cfg(target_os = "linux")]
            bytes_written,
        }
    }

    /// Apply one-time platform-specific optimizations to the file descriptor.
    fn apply_platform_hints(file: &std::fs::File) {
        #[cfg(target_os = "linux")]
        Self::apply_linux_hints(file);
        #[cfg(target_os = "macos")]
        Self::apply_macos_hints(file);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let _ = file;
    }

    /// Linux: pre-allocate space with `fallocate` and set sequential advice.
    #[cfg(target_os = "linux")]
    fn apply_linux_hints(file: &std::fs::File) {
        use rustix::fs::{Advice, FallocateFlags, fadvise, fallocate};

        // Pre-allocate disk space to avoid metadata updates on every
        // extending write.  KEEP_SIZE means the visible file size is not
        // changed — only the underlying blocks are reserved.
        let _ = fallocate(file, FallocateFlags::KEEP_SIZE, 0, PREALLOC_BYTES);

        // Tell the kernel we're doing sequential writes so it can optimize
        // read-ahead and page cache management.
        let _ = fadvise(file, 0, None, Advice::Sequential);
    }

    /// macOS: set `F_NOCACHE` and pre-allocate with `F_PREALLOCATE`.
    #[cfg(target_os = "macos")]
    fn apply_macos_hints(file: &std::fs::File) {
        use std::os::unix::io::AsRawFd;

        let fd = file.as_raw_fd();

        // F_NOCACHE bypasses the Unified Buffer Cache for this descriptor so
        // write-only log data does not linger in process-visible cache.
        // SAFETY: `fd` is owned by `file`, and F_NOCACHE with arg=1 is an
        // advisory descriptor hint that is safe to ignore on failure.
        unsafe {
            let _ = libc::fcntl(fd, libc::F_NOCACHE, 1i32);
        }

        // F_PREALLOCATE is macOS's fallocate equivalent. It reserves space
        // without changing the visible file length.
        // SAFETY: `fd` is owned by `file`; `store` is fully initialized before
        // the kernel reads it, and fcntl does not retain the pointer.
        unsafe {
            let mut store: libc::fstore_t = std::mem::zeroed();
            store.fst_flags = libc::F_ALLOCATECONTIG;
            store.fst_posmode = libc::F_PEOFPOSMODE;
            store.fst_offset = 0;
            store.fst_length = PREALLOC_BYTES as libc::off_t;
            let ret = libc::fcntl(fd, libc::F_PREALLOCATE, &store);
            if ret == -1 {
                store.fst_flags = libc::F_ALLOCATEALL;
                let _ = libc::fcntl(fd, libc::F_PREALLOCATE, &store);
            }
        }
    }

    /// Linux: advise the kernel to drop page cache for already-written data.
    ///
    /// This prevents the forwarder from bloating the host's page cache with
    /// write-only log data that will never be re-read by this process.
    #[cfg(target_os = "linux")]
    fn advise_dontneed(&self, offset: u64, len: u64) {
        use std::num::NonZeroU64;

        use rustix::fs::{Advice, fadvise};

        let Some(len) = NonZeroU64::new(len) else {
            return;
        };
        let _ = fadvise(&self.file, offset, Some(len), Advice::DontNeed);
    }
}

impl BatchWriter for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        use std::io::Write;
        self.file.write_all(buf)?;

        // Tell the OS to drop the pages we just wrote — we won't re-read them.
        #[cfg(target_os = "linux")]
        {
            let offset = self.bytes_written;
            self.bytes_written += buf.len() as u64;
            self.advise_dontneed(offset, buf.len() as u64);
        }

        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        use std::io::Write;
        self.file.flush()?;
        self.file.sync_data()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        use std::io::Write;
        self.file.flush()?;
        self.file.sync_data()
    }
}

// ---------------------------------------------------------------------------
// JsonBatchSerializer
// ---------------------------------------------------------------------------

use crate::stdout::{StdoutFormat, StdoutSink};

/// A [`BatchSerializer`] that produces JSON-lines or text output.
///
/// Wraps the same serialization logic as `StdoutSink::write_batch_to`.
pub struct JsonBatchSerializer {
    inner: StdoutSink,
}

impl JsonBatchSerializer {
    /// Create with a custom message field name.
    pub fn with_message_field(
        name: String,
        format: StdoutFormat,
        message_field: String,
        stats: Arc<ComponentStats>,
    ) -> Self {
        Self {
            inner: StdoutSink::with_message_field(name, format, message_field, stats),
        }
    }
}

impl BatchSerializer for JsonBatchSerializer {
    fn serialize(
        &mut self,
        batch: &RecordBatch,
        metadata: &BatchMetadata,
        buf: &mut Vec<u8>,
    ) -> io::Result<u64> {
        self.inner
            .write_batch_to(batch, metadata, buf)
            .map(|n| n as u64)
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use arrow::datatypes::Schema;
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;

    use super::*;

    struct FixedSerializer {
        rows: u64,
        payload: &'static [u8],
    }

    impl BatchSerializer for FixedSerializer {
        fn serialize(
            &mut self,
            _batch: &RecordBatch,
            _metadata: &BatchMetadata,
            buf: &mut Vec<u8>,
        ) -> io::Result<u64> {
            buf.extend_from_slice(self.payload);
            Ok(self.rows)
        }

        fn name(&self) -> &str {
            "fixed"
        }
    }

    struct FailsOnceSerializer {
        failed_once: bool,
        rows: u64,
        payload: &'static [u8],
    }

    impl BatchSerializer for FailsOnceSerializer {
        fn serialize(
            &mut self,
            _batch: &RecordBatch,
            _metadata: &BatchMetadata,
            buf: &mut Vec<u8>,
        ) -> io::Result<u64> {
            if self.failed_once {
                buf.extend_from_slice(self.payload);
                Ok(self.rows)
            } else {
                self.failed_once = true;
                Err(io::Error::other("encode failed"))
            }
        }

        fn name(&self) -> &str {
            "fails-once"
        }
    }

    #[derive(Clone, Debug)]
    struct ScriptStep {
        serialize_ok: bool,
        write_ok: bool,
        rows: u64,
        payload: Vec<u8>,
    }

    #[derive(Clone, Debug)]
    enum ScriptOp {
        Send(ScriptStep),
        Flush,
    }

    type SharedWritePlan = Arc<Mutex<VecDeque<ScriptStep>>>;

    struct ScriptedSerializer {
        steps: VecDeque<ScriptStep>,
        write_plan: SharedWritePlan,
    }

    impl BatchSerializer for ScriptedSerializer {
        fn serialize(
            &mut self,
            _batch: &RecordBatch,
            _metadata: &BatchMetadata,
            buf: &mut Vec<u8>,
        ) -> io::Result<u64> {
            let step = self
                .steps
                .pop_front()
                .ok_or_else(|| io::Error::other("missing scripted serialize step"))?;
            if !step.serialize_ok {
                return Err(io::Error::other("scripted encode failure"));
            }
            buf.extend_from_slice(&step.payload);
            self.write_plan
                .lock()
                .expect("scripted write plan mutex")
                .push_back(step.clone());
            Ok(step.rows)
        }

        fn name(&self) -> &str {
            "scripted"
        }
    }

    struct RecordingWriter {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl BatchWriter for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<()> {
            self.writes
                .lock()
                .expect("recording writer mutex")
                .push(buf.to_vec());
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct ScriptedWriter {
        write_plan: SharedWritePlan,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl BatchWriter for ScriptedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<()> {
            let step = self
                .write_plan
                .lock()
                .expect("scripted write plan mutex")
                .pop_front()
                .ok_or_else(|| io::Error::other("missing scripted write step"))?;
            if buf != step.payload.as_slice() {
                return Err(io::Error::other("scripted payload mismatch"));
            }
            if step.write_ok {
                self.writes
                    .lock()
                    .expect("scripted writes mutex")
                    .push(buf.to_vec());
                Ok(())
            } else {
                Err(io::Error::other("scripted write failure"))
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailingWriter;

    impl BatchWriter for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct DelayedThenFailWriter {
        release_first: std_mpsc::Receiver<()>,
        writes: usize,
    }

    impl BatchWriter for DelayedThenFailWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<()> {
            self.writes += 1;
            if self.writes == 1 {
                self.release_first
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(io::Error::other)?;
                Ok(())
            } else {
                Err(io::Error::other("second write failed"))
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn empty_batch() -> RecordBatch {
        RecordBatch::new_empty(Arc::new(Schema::empty()))
    }

    fn metadata() -> BatchMetadata {
        BatchMetadata {
            resource_attrs: Arc::default(),
            observed_time_ns: 0,
        }
    }

    #[tokio::test]
    async fn send_batch_returns_only_after_writer_success() {
        let stats = Arc::new(ComponentStats::new());
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut sink = PipelinedSink::new(
            FixedSerializer {
                rows: 2,
                payload: b"ok\n",
            },
            RecordingWriter {
                writes: Arc::clone(&writes),
            },
            Arc::clone(&stats),
            PipelineConfig {
                num_buffers: 1,
                buf_capacity: 16,
            },
        )
        .expect("sink");

        let batch = empty_batch();
        let result = sink.send_batch(&batch, &metadata()).await;

        assert!(result.is_ok(), "result: {result:?}");
        assert_eq!(
            writes.lock().expect("recording writer mutex").as_slice(),
            &[b"ok\n".to_vec()]
        );
        assert_eq!(stats.lines_total.load(Ordering::Relaxed), 2);
        assert_eq!(stats.bytes_total.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn send_batch_reports_writer_error_without_success_counters() {
        let stats = Arc::new(ComponentStats::new());
        let mut sink = PipelinedSink::new(
            FixedSerializer {
                rows: 2,
                payload: b"lost\n",
            },
            FailingWriter,
            Arc::clone(&stats),
            PipelineConfig {
                num_buffers: 1,
                buf_capacity: 16,
            },
        )
        .expect("sink");

        let batch = empty_batch();
        let result = sink.send_batch(&batch, &metadata()).await;

        assert!(
            matches!(result, SendResult::IoError(_)),
            "result: {result:?}"
        );
        assert_eq!(stats.lines_total.load(Ordering::Relaxed), 0);
        assert_eq!(stats.bytes_total.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn serialization_error_keeps_buffer_available_for_retry() {
        let stats = Arc::new(ComponentStats::new());
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut sink = PipelinedSink::new(
            FailsOnceSerializer {
                failed_once: false,
                rows: 1,
                payload: b"retry\n",
            },
            RecordingWriter {
                writes: Arc::clone(&writes),
            },
            Arc::clone(&stats),
            PipelineConfig {
                num_buffers: 1,
                buf_capacity: 16,
            },
        )
        .expect("sink");

        let batch = empty_batch();
        let first = sink.send_batch(&batch, &metadata()).await;
        assert!(matches!(first, SendResult::IoError(_)), "result: {first:?}");

        let second = sink.send_batch(&batch, &metadata()).await;
        assert!(second.is_ok(), "result: {second:?}");
        assert_eq!(
            writes.lock().expect("recording writer mutex").as_slice(),
            &[b"retry\n".to_vec()]
        );
        assert_eq!(stats.lines_total.load(Ordering::Relaxed), 1);
        assert_eq!(stats.bytes_total.load(Ordering::Relaxed), 6);
    }

    #[tokio::test]
    async fn timed_out_send_does_not_satisfy_next_batch_ack() {
        let stats = Arc::new(ComponentStats::new());
        let (release_first_tx, release_first_rx) = std_mpsc::channel();
        let mut sink = PipelinedSink::new(
            FixedSerializer {
                rows: 1,
                payload: b"batch\n",
            },
            DelayedThenFailWriter {
                release_first: release_first_rx,
                writes: 0,
            },
            Arc::clone(&stats),
            PipelineConfig {
                num_buffers: 1,
                buf_capacity: 16,
            },
        )
        .expect("sink");

        let batch = empty_batch();
        let first = tokio::time::timeout(
            Duration::from_millis(10),
            sink.send_batch(&batch, &metadata()),
        )
        .await;
        assert!(first.is_err(), "first send should time out");

        let second =
            tokio::time::timeout(Duration::from_secs(1), sink.send_batch(&batch, &metadata()))
                .await
                .expect("buffer acquisition should not block the Tokio worker");
        assert!(
            matches!(second, SendResult::IoError(ref error) if error.kind() == io::ErrorKind::WouldBlock),
            "second send should report buffer backpressure, got {second:?}"
        );

        release_first_tx.send(()).expect("release first write");
        let third = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let result = sink.send_batch(&batch, &metadata()).await;
                if !matches!(result, SendResult::IoError(ref error) if error.kind() == io::ErrorKind::WouldBlock)
                {
                    break result;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("writer should recycle the first timed-out buffer");
        assert!(
            matches!(third, SendResult::IoError(_)),
            "third send must receive its own write failure, got {third:?}"
        );
        tokio::time::timeout(Duration::from_secs(1), sink.shutdown())
            .await
            .expect("shutdown should not hang")
            .expect("shutdown");
        assert_eq!(stats.lines_total.load(Ordering::Relaxed), 0);
        assert_eq!(stats.bytes_total.load(Ordering::Relaxed), 0);
    }

    fn script_step_strategy() -> impl Strategy<Value = ScriptStep> {
        (
            any::<bool>(),
            any::<bool>(),
            0u64..8,
            prop::collection::vec(any::<u8>(), 0..64),
        )
            .prop_map(|(serialize_ok, write_ok, rows, payload)| ScriptStep {
                serialize_ok,
                write_ok,
                rows,
                payload,
            })
    }

    fn script_op_strategy() -> impl Strategy<Value = ScriptOp> {
        prop_oneof![
            script_step_strategy().prop_map(ScriptOp::Send),
            Just(ScriptOp::Flush),
        ]
    }

    proptest! {
        #[test]
        fn arbitrary_send_and_flush_sequences_count_only_acked_writes(
            ops in prop::collection::vec(script_op_strategy(), 1..32)
        ) {
            let send_steps = ops
                .iter()
                .filter_map(|op| match op {
                    ScriptOp::Send(step) => Some(step.clone()),
                    ScriptOp::Flush => None,
                })
                .collect();
            let write_plan = Arc::new(Mutex::new(VecDeque::new()));
            let writes = Arc::new(Mutex::new(Vec::new()));
            let stats = Arc::new(ComponentStats::new());
            let mut sink = PipelinedSink::new(
                ScriptedSerializer {
                    steps: send_steps,
                    write_plan: Arc::clone(&write_plan),
                },
                ScriptedWriter {
                    write_plan,
                    writes: Arc::clone(&writes),
                },
                Arc::clone(&stats),
                PipelineConfig {
                    num_buffers: 1,
                    buf_capacity: 64,
                },
            )
            .expect("sink");
            let batch = empty_batch();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            let (expected_writes, expected_rows, expected_bytes) = runtime.block_on(async {
                let mut expected_writes = Vec::new();
                let mut expected_rows = 0u64;
                let mut expected_bytes = 0u64;

                for op in &ops {
                    match op {
                        ScriptOp::Send(step) => {
                            let result = sink.send_batch(&batch, &metadata()).await;
                            let should_succeed = step.serialize_ok && step.write_ok;
                            prop_assert_eq!(result.is_ok(), should_succeed);
                            if should_succeed {
                                expected_rows += step.rows;
                                expected_bytes += step.payload.len() as u64;
                                expected_writes.push(step.payload.clone());
                            }
                        }
                        ScriptOp::Flush => {
                            sink.flush().await.expect("flush");
                        }
                    }
                }

                sink.shutdown().await.expect("shutdown");
                Ok::<_, TestCaseError>((expected_writes, expected_rows, expected_bytes))
            })?;

            let actual_writes = writes.lock().expect("scripted writes mutex").clone();
            prop_assert_eq!(actual_writes, expected_writes);
            prop_assert_eq!(stats.lines_total.load(Ordering::Relaxed), expected_rows);
            prop_assert_eq!(stats.bytes_total.load(Ordering::Relaxed), expected_bytes);
        }
    }
}
