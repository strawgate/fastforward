//! `Sink` implementation that sends NDJSON over `turmoil::net::TcpStream`.
//!
//! This is a test-only sink used to exercise real network failure handling
//! through the worker pool retry logic. It connects lazily on the first
//! `send_batch` call and reconnects on error.
//!
//! Turmoil's simulated TCP has no retransmission: when a link is randomly
//! partitioned (`fail_rate`), in-flight segments are silently discarded and
//! the receiver stalls forever waiting for the missing sequence number, while
//! the sender sees every write succeed. Tests that inject random link failures
//! must therefore enable [`TurmoilTcpSink::with_ack_timeout`] (paired with
//! `run_acking_tcp_server`) so lost data surfaces as a send error and the
//! worker pool retries the batch, the way a real transport would.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arrow::record_batch::RecordBatch;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use ffwd_output::BatchMetadata;
use ffwd_output::sink::{SendResult, Sink};
use ffwd_output::{build_col_infos, write_row_json};

/// A Sink that writes NDJSON lines over a turmoil-simulated TCP connection.
///
/// Connects lazily on the first `send_batch` call. If the connection breaks,
/// the next call will attempt to reconnect.
pub struct TurmoilTcpSink {
    host: String,
    port: u16,
    stream: Option<turmoil::net::TcpStream>,
    delivered_rows: Arc<AtomicU64>,
    /// When set, every batch must be acknowledged (one byte per line) by the
    /// server within this duration, or the connection is dropped and the
    /// batch reported as an `IoError` so the worker pool retries it.
    ack_timeout: Option<Duration>,
}

#[allow(dead_code)]
impl TurmoilTcpSink {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            host: host.to_string(),
            port,
            stream: None,
            delivered_rows: Arc::new(AtomicU64::new(0)),
            ack_timeout: None,
        }
    }

    /// Require a per-line acknowledgement from the server for every batch.
    ///
    /// Use with `run_acking_tcp_server`. Needed whenever the simulated link
    /// can randomly drop segments, because turmoil TCP never retransmits.
    pub fn with_ack_timeout(mut self, timeout: Duration) -> Self {
        self.ack_timeout = Some(timeout);
        self
    }

    pub fn delivered_counter(&self) -> Arc<AtomicU64> {
        self.delivered_rows.clone()
    }

    async fn ensure_connected(&mut self) -> io::Result<()> {
        if self.stream.is_none() {
            let addr = (self.host.as_str(), self.port);
            let connect = turmoil::net::TcpStream::connect(addr);
            let stream = match self.ack_timeout {
                // A dropped SYN would otherwise leave connect pending forever.
                Some(limit) => tokio::time::timeout(limit, connect)
                    .await
                    .map_err(|elapsed| io::Error::new(io::ErrorKind::TimedOut, elapsed))??,
                None => connect.await?,
            };
            self.stream = Some(stream);
        }
        Ok(())
    }

    /// Write the whole batch, then wait for one ack byte per line.
    async fn send_acked(
        stream: &mut turmoil::net::TcpStream,
        payload: &[u8],
        rows: usize,
        limit: Duration,
    ) -> io::Result<()> {
        let exchange = async {
            stream.write_all(payload).await?;
            let mut acks = vec![0u8; rows];
            stream.read_exact(&mut acks).await?;
            Ok::<(), io::Error>(())
        };
        tokio::time::timeout(limit, exchange)
            .await
            .map_err(|elapsed| io::Error::new(io::ErrorKind::TimedOut, elapsed))?
    }
}

impl Sink for TurmoilTcpSink {
    fn send_batch<'a>(
        &'a mut self,
        batch: &'a RecordBatch,
        _metadata: &'a BatchMetadata,
    ) -> Pin<Box<dyn Future<Output = SendResult> + Send + 'a>> {
        Box::pin(async move {
            // Connect (or reconnect) if needed.
            if let Err(e) = self.ensure_connected().await {
                self.stream = None;
                return SendResult::IoError(e);
            }

            // Serialize each row as a JSON line.
            let cols = build_col_infos(batch);

            if let Some(limit) = self.ack_timeout {
                let mut payload = Vec::new();
                for row in 0..batch.num_rows() {
                    if let Err(e) = write_row_json(batch, row, &cols, &mut payload, false) {
                        return SendResult::IoError(io::Error::other(e.to_string()));
                    }
                    payload.push(b'\n');
                }
                let stream = self.stream.as_mut().expect("connected above");
                if let Err(e) = Self::send_acked(stream, &payload, batch.num_rows(), limit).await {
                    // Lost segment or ack: abandon this connection so the
                    // retry starts on a fresh one.
                    self.stream = None;
                    return SendResult::IoError(e);
                }
                self.delivered_rows
                    .fetch_add(batch.num_rows() as u64, Ordering::Relaxed);
                return SendResult::Ok;
            }

            let mut buf = Vec::new();
            for row in 0..batch.num_rows() {
                buf.clear();
                if let Err(e) = write_row_json(batch, row, &cols, &mut buf, false) {
                    return SendResult::IoError(io::Error::other(e.to_string()));
                }
                buf.push(b'\n');

                let stream = self.stream.as_mut().unwrap();
                if let Err(e) = stream.write_all(&buf).await {
                    // Connection broken — drop it so next call reconnects.
                    self.stream = None;
                    return SendResult::IoError(e);
                }
            }

            self.delivered_rows
                .fetch_add(batch.num_rows() as u64, Ordering::Relaxed);
            SendResult::Ok
        })
    }

    fn flush(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async move {
            if let Some(stream) = self.stream.as_mut() {
                stream.flush().await?;
            }
            Ok(())
        })
    }

    fn name(&self) -> &str {
        "turmoil-tcp"
    }

    fn shutdown(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async move {
            if let Some(mut stream) = self.stream.take() {
                let _ = stream.shutdown().await;
            }
            Ok(())
        })
    }
}

/// Factory that creates `TurmoilTcpSink` instances for multi-worker testing.
#[allow(dead_code)]
pub struct TurmoilTcpSinkFactory {
    host: String,
    port: u16,
    delivered_rows: Arc<AtomicU64>,
}

#[allow(dead_code)]
impl TurmoilTcpSinkFactory {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            host: host.to_string(),
            port,
            delivered_rows: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn delivered_counter(&self) -> Arc<AtomicU64> {
        self.delivered_rows.clone()
    }
}

impl ffwd_output::SinkFactory for TurmoilTcpSinkFactory {
    fn create(&self) -> io::Result<Box<dyn Sink>> {
        let mut sink = TurmoilTcpSink::new(&self.host, self.port);
        sink.delivered_rows = self.delivered_rows.clone();
        Ok(Box::new(sink))
    }

    fn name(&self) -> &str {
        "turmoil-tcp-factory"
    }

    fn is_single_use(&self) -> bool {
        false
    }
}
