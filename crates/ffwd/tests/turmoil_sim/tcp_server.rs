//! Controlled TCP server for turmoil network simulation tests.
//!
//! The server accepts NDJSON over TCP and counts received lines and connections.
//! It runs inside a `sim.host()` closure and is designed to be crashed/bounced
//! by the turmoil framework.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Handle for inspecting server state from outside the turmoil host.
#[derive(Clone)]
pub struct TcpServerHandle {
    pub received_lines: Arc<AtomicU64>,
    pub connection_count: Arc<AtomicU64>,
    /// Distinct line payloads seen (populated by `run_acking_tcp_server`
    /// only), so tests can assert every line arrived despite retries.
    pub distinct_lines: Arc<Mutex<HashSet<Vec<u8>>>>,
}

impl TcpServerHandle {
    pub fn new() -> Self {
        Self {
            received_lines: Arc::new(AtomicU64::new(0)),
            connection_count: Arc::new(AtomicU64::new(0)),
            distinct_lines: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

/// Run a TCP server that accepts NDJSON lines.
///
/// Call this inside a `sim.host()` closure. The server loops forever,
/// accepting connections and counting newline-delimited lines.
pub async fn run_tcp_server(
    port: u16,
    handle: TcpServerHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = turmoil::net::TcpListener::bind(("0.0.0.0", port)).await?;
    loop {
        let (mut stream, _addr) = listener.accept().await?;
        handle.connection_count.fetch_add(1, Ordering::Relaxed);
        let line_counter = handle.received_lines.clone();
        // Spawn a task per connection so we can accept multiple concurrently.
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let lines = buf[..n].iter().filter(|&&b| b == b'\n').count();
                        line_counter.fetch_add(lines as u64, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            }
        });
    }
}

/// Run a TCP server that acknowledges every received line with one byte.
///
/// Pair with `TurmoilTcpSink::with_ack_timeout`. Counts every received line
/// (retries can deliver a line more than once) and records distinct lines.
pub async fn run_acking_tcp_server(
    port: u16,
    handle: TcpServerHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = turmoil::net::TcpListener::bind(("0.0.0.0", port)).await?;
    loop {
        let (mut stream, _addr) = listener.accept().await?;
        handle.connection_count.fetch_add(1, Ordering::Relaxed);
        let handle = handle.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let mut pending: Vec<u8> = Vec::new();
            loop {
                let n = match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                pending.extend_from_slice(&buf[..n]);
                let mut complete = 0usize;
                while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=pos).collect();
                    handle
                        .distinct_lines
                        .lock()
                        .expect("distinct_lines lock")
                        .insert(line);
                    complete += 1;
                }
                if complete > 0 {
                    handle
                        .received_lines
                        .fetch_add(complete as u64, Ordering::Relaxed);
                    if stream.write_all(&vec![b'k'; complete]).await.is_err() {
                        break;
                    }
                }
            }
        });
    }
}
