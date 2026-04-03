//! TCP input source. Listens on a TCP socket and produces newline-delimited
//! log lines from connected clients. Multiple concurrent connections supported.

use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use socket2::SockRef;

use crate::input::{InputEvent, InputSource};

/// Maximum number of concurrent TCP client connections.
const MAX_CLIENTS: usize = 1024;

/// Per-poll read buffer size (64 KiB). Shared across all connections within a
/// single `poll` call; data is copied into per-connection `line_buf`s
/// immediately, so one moderate buffer is sufficient.
const READ_BUF_SIZE: usize = 64 * 1024;

/// Default disconnect timeout for idle clients (no data received).
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum bytes a client may send without a newline before we disconnect them.
/// Prevents a misbehaving sender from consuming unbounded memory.
const MAX_LINE_LENGTH: usize = 1024 * 1024; // 1 MiB

/// A connected TCP client with an associated last-data timestamp.
struct Client {
    stream: TcpStream,
    last_data: Instant,
    /// Per-connection partial line buffer. Complete lines (ending in `\n`) are
    /// extracted each poll cycle; the remainder stays here until the next read
    /// or until the client disconnects (at which point it is flushed with a
    /// synthetic newline).
    line_buf: Vec<u8>,
}

/// TCP input that accepts connections and reads newline-delimited data.
pub struct TcpInput {
    name: String,
    listener: TcpListener,
    clients: Vec<Client>,
    buf: Vec<u8>,
    idle_timeout: Duration,
    /// Total connections accepted since creation (never decreases).
    connections_accepted: u64,
}

impl TcpInput {
    /// Bind to `addr` (e.g. "0.0.0.0:5140") with the default idle timeout.
    pub fn new(name: impl Into<String>, addr: &str) -> io::Result<Self> {
        Self::with_idle_timeout(name, addr, DEFAULT_IDLE_TIMEOUT)
    }

    /// Bind to `addr` with a custom idle timeout.
    pub fn with_idle_timeout(
        name: impl Into<String>,
        addr: &str,
        idle_timeout: Duration,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            name: name.into(),
            listener,
            clients: Vec::new(),
            buf: vec![0u8; READ_BUF_SIZE],
            idle_timeout,
            connections_accepted: 0,
        })
    }

    /// Returns the local address this listener is bound to.
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Returns the number of currently tracked client connections.
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Returns the total number of connections accepted since creation.
    /// Monotonically increasing — useful for tests that need to verify a
    /// connection was accepted even if it was immediately disconnected.
    #[cfg(test)]
    pub fn connections_accepted(&self) -> u64 {
        self.connections_accepted
    }
}

impl InputSource for TcpInput {
    fn poll(&mut self) -> io::Result<Vec<InputEvent>> {
        // Accept new connections up to the limit.
        loop {
            if self.clients.len() >= MAX_CLIENTS {
                // Drain (and drop) any pending connections beyond the limit so
                // the kernel accept queue does not fill up and stall.
                match self.listener.accept() {
                    Ok((_stream, _addr)) => continue, // dropped immediately
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break, // transient accept error, not fatal
                }
            }
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    stream.set_nonblocking(true)?;

                    // Enable TCP keepalive so we detect dead peers promptly.
                    // Use SockRef to borrow the fd — no clone, no leak.
                    let sock_ref = SockRef::from(&stream);
                    let _ = sock_ref.set_keepalive(true);
                    let keepalive = socket2::TcpKeepalive::new()
                        .with_time(Duration::from_secs(60))
                        .with_interval(Duration::from_secs(10));
                    let _ = sock_ref.set_tcp_keepalive(&keepalive);

                    self.connections_accepted += 1;
                    self.clients.push(Client {
                        stream,
                        last_data: Instant::now(),
                        line_buf: Vec::new(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {
                    // Peer reset before we accepted — harmless, keep going.
                }
                Err(e) => return Err(e),
            }
        }

        // Read from all clients into per-connection line buffers.
        let now = Instant::now();
        // Track which connections are dead using a bitmap for O(1) lookup.
        let mut alive = vec![true; self.clients.len()];

        for (i, client) in self.clients.iter_mut().enumerate() {
            let mut got_data = false;
            loop {
                match client.stream.read(&mut self.buf) {
                    Ok(0) => {
                        // Clean EOF.
                        alive[i] = false;
                        break;
                    }
                    Ok(n) => {
                        let chunk = &self.buf[..n];
                        client.last_data = now;
                        got_data = true;

                        client.line_buf.extend_from_slice(chunk);

                        // Check the line buffer size for max-line-length.
                        // Use bytes after the last newline in the buffer as
                        // the current incomplete line length.
                        let bytes_since_newline =
                            if let Some(last_nl) = memchr::memrchr(b'\n', &client.line_buf) {
                                client.line_buf.len() - last_nl - 1
                            } else {
                                client.line_buf.len()
                            };
                        if bytes_since_newline >= MAX_LINE_LENGTH {
                            alive[i] = false;
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {
                        // Peer sent RST — treat as a close.
                        alive[i] = false;
                        break;
                    }
                    Err(_) => {
                        // Any other read error — drop this connection.
                        alive[i] = false;
                        break;
                    }
                }
            }
            // Check idle AFTER reading — data may have arrived since last poll.
            if !got_data && alive[i] && now.duration_since(client.last_data) > self.idle_timeout {
                alive[i] = false;
            }
        }

        // Collect complete lines from alive clients, and flush partial lines
        // from dead clients (issue #804: disconnect flushes partial line).
        let mut all_data = Vec::new();

        for (i, client) in self.clients.iter_mut().enumerate() {
            if alive[i] {
                // Extract only complete lines (up to and including the last `\n`).
                if let Some(last_nl) = memchr::memrchr(b'\n', &client.line_buf) {
                    all_data.extend(client.line_buf.drain(..=last_nl));
                }
            } else {
                // Dead client: flush any remaining partial line with a
                // synthetic newline so it is not lost.
                if !client.line_buf.is_empty() {
                    client.line_buf.push(b'\n');
                    all_data.extend_from_slice(&client.line_buf);
                    client.line_buf.clear();
                }
            }
        }

        // Remove dead connections, preserving order of remaining ones.
        // `retain` is cleaner than manual swap_remove with index tracking.
        let mut idx = 0;
        self.clients.retain(|_| {
            let keep = alive[idx];
            idx += 1;
            keep
        });

        let mut events = Vec::new();
        if !all_data.is_empty() {
            events.push(InputEvent::Data {
                bytes: all_data,
                source_id: None,
            });
        }

        Ok(events)
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpStream as StdTcpStream;

    #[test]
    fn receives_tcp_data() {
        let input = TcpInput::new("test", "127.0.0.1:0").unwrap();
        let addr = input.listener.local_addr().unwrap();

        let mut client = StdTcpStream::connect(addr).unwrap();
        client.write_all(b"{\"msg\":\"hello\"}\n").unwrap();
        client.write_all(b"{\"msg\":\"world\"}\n").unwrap();
        client.flush().unwrap();

        std::thread::sleep(Duration::from_millis(50));

        let mut input = input; // make mutable
        let events = input.poll().unwrap();

        // Should have accepted the connection and read data.
        assert_eq!(events.len(), 1);
        if let InputEvent::Data { bytes, .. } = &events[0] {
            let text = String::from_utf8_lossy(bytes);
            assert!(text.contains("hello"), "got: {text}");
            assert!(text.contains("world"), "got: {text}");
        }
    }

    #[test]
    fn handles_disconnect() {
        let mut input = TcpInput::new("test", "127.0.0.1:0").unwrap();
        let addr = input.listener.local_addr().unwrap();

        {
            let mut client = StdTcpStream::connect(addr).unwrap();
            client.write_all(b"line1\n").unwrap();
        } // client drops here → connection closed

        std::thread::sleep(Duration::from_millis(50));

        let events = input.poll().unwrap();
        assert_eq!(events.len(), 1);

        // Second poll should clean up the closed connection.
        let events = input.poll().unwrap();
        assert!(events.is_empty());
        assert!(input.clients.is_empty());
    }

    #[test]
    fn tcp_idle_timeout() {
        // Use a very short idle timeout so the test runs fast.
        let mut input =
            TcpInput::with_idle_timeout("test", "127.0.0.1:0", Duration::from_millis(200)).unwrap();
        let addr = input.local_addr().unwrap();

        // Connect but send nothing.
        let _client = StdTcpStream::connect(addr).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // First poll: accept the connection.
        let _ = input.poll().unwrap();
        assert_eq!(input.client_count(), 1, "should have 1 client after accept");

        // Wait longer than the idle timeout.
        std::thread::sleep(Duration::from_millis(300));

        // Next poll should evict the idle connection.
        let _ = input.poll().unwrap();
        assert_eq!(
            input.client_count(),
            0,
            "idle client should have been disconnected"
        );
    }

    #[test]
    fn tcp_max_line_length() {
        let mut input = TcpInput::new("test", "127.0.0.1:0").unwrap();
        let addr = input.local_addr().unwrap();

        // Spawn the writer in a background thread because write_all of >1MB
        // will block until the reader drains the kernel buffer.
        let writer = std::thread::spawn(move || {
            let mut client = StdTcpStream::connect(addr).unwrap();
            let big = vec![b'A'; MAX_LINE_LENGTH + 1];
            // Ignore errors — the server may reset the connection once
            // the limit is exceeded, causing a broken-pipe on our side.
            let _ = client.write_all(&big);
        });

        // Poll until the writer finishes (connection accepted and data read) or
        // deadline. The accept and disconnect may happen in the same poll() call
        // when the data arrives faster than the poll loop iterates, so we track
        // total connections_accepted() rather than the transient client_count().
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let _ = input.poll().unwrap();
            if input.connections_accepted() > 0 && input.client_count() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let _ = writer.join();

        assert!(
            input.connections_accepted() > 0,
            "server must have accepted the connection"
        );
        assert_eq!(
            input.client_count(),
            0,
            "client exceeding max_line_length should be disconnected"
        );
    }

    #[test]
    fn tcp_max_line_length_exact_boundary() {
        // A line of exactly MAX_LINE_LENGTH bytes (content only, excluding \n)
        // must be rejected — the check is `>=`, so the boundary is exclusive.
        let mut input = TcpInput::new("test", "127.0.0.1:0").unwrap();
        let addr = input.local_addr().unwrap();

        let writer = std::thread::spawn(move || {
            let mut client = StdTcpStream::connect(addr).unwrap();
            // Exactly MAX_LINE_LENGTH content bytes followed by a newline.
            let mut line = vec![b'A'; MAX_LINE_LENGTH];
            line.push(b'\n');
            let _ = client.write_all(&line);
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let _ = input.poll().unwrap();
            if input.connections_accepted() > 0 && input.client_count() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let _ = writer.join();

        assert!(
            input.connections_accepted() > 0,
            "server must have accepted the connection"
        );
        assert_eq!(
            input.client_count(),
            0,
            "client sending a line of exactly MAX_LINE_LENGTH bytes should be disconnected"
        );
    }

    #[test]
    fn tcp_connection_storm() {
        let mut input = TcpInput::new("test", "127.0.0.1:0").unwrap();
        let addr = input.local_addr().unwrap();

        // Rapidly connect and disconnect 100 times.
        for _ in 0..100 {
            let _ = StdTcpStream::connect(addr).unwrap();
            // Immediately dropped — connection closed.
        }

        std::thread::sleep(Duration::from_millis(100));

        // Poll several times to accept and then clean up all connections.
        for _ in 0..20 {
            let _ = input.poll().unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(
            input.client_count(),
            0,
            "all storm connections should be cleaned up (no fd leak)"
        );
    }

    #[test]
    fn tcp_per_connection_isolation() {
        // Two clients sending partial lines should NOT have their data merged.
        // Before the fix, Client A's "hello " and Client B's "world\n" would
        // combine into "hello world\n" — a corrupted line from neither client.
        let mut input = TcpInput::new("test", "127.0.0.1:0").unwrap();
        let addr = input.listener.local_addr().unwrap();

        let mut client_a = StdTcpStream::connect(addr).unwrap();
        let mut client_b = StdTcpStream::connect(addr).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // Accept both connections.
        let _ = input.poll().unwrap();

        // Client A sends a partial line (no newline).
        client_a.write_all(b"from_a_partial").unwrap();
        client_a.flush().unwrap();

        // Client B sends a complete line.
        client_b.write_all(b"from_b_complete\n").unwrap();
        client_b.flush().unwrap();

        std::thread::sleep(Duration::from_millis(50));

        let events = input.poll().unwrap();
        let mut all_bytes = Vec::new();
        for event in &events {
            if let InputEvent::Data { bytes, .. } = event {
                all_bytes.extend_from_slice(bytes);
            }
        }
        let text = String::from_utf8_lossy(&all_bytes);

        // Client B's complete line should appear.
        assert!(
            text.contains("from_b_complete\n"),
            "expected Client B's complete line, got: {text}"
        );

        // Client A's partial data must NOT appear yet (no newline sent).
        assert!(
            !text.contains("from_a_partial"),
            "Client A's partial line should be buffered, not emitted; got: {text}"
        );

        // Now Client A completes its line.
        client_a.write_all(b"_done\n").unwrap();
        client_a.flush().unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let events = input.poll().unwrap();
        let mut all_bytes = Vec::new();
        for event in &events {
            if let InputEvent::Data { bytes, .. } = event {
                all_bytes.extend_from_slice(bytes);
            }
        }
        let text = String::from_utf8_lossy(&all_bytes);

        // Now Client A's full line should appear, correctly reassembled.
        assert!(
            text.contains("from_a_partial_done\n"),
            "expected Client A's reassembled line, got: {text}"
        );

        // Verify no cross-contamination: "from_a_partialfrom_b" should never appear.
        assert!(
            !text.contains("from_a_partialfrom_b"),
            "cross-connection data corruption detected: {text}"
        );
    }

    #[test]
    fn tcp_disconnect_flushes_partial_line() {
        // When a client disconnects with a partial line (no trailing newline),
        // the data should be emitted with a synthetic newline — not lost.
        let mut input = TcpInput::new("test", "127.0.0.1:0").unwrap();
        let addr = input.listener.local_addr().unwrap();

        {
            let mut client = StdTcpStream::connect(addr).unwrap();
            // Send partial line without newline, then disconnect.
            client.write_all(b"partial_no_newline").unwrap();
            client.flush().unwrap();
            std::thread::sleep(Duration::from_millis(50));
        } // client drops here → connection closed

        std::thread::sleep(Duration::from_millis(50));

        // Poll until we see the data (may take a couple of polls for accept +
        // read + EOF detection).
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut all_bytes = Vec::new();
        while Instant::now() < deadline {
            let events = input.poll().unwrap();
            for event in &events {
                if let InputEvent::Data { bytes, .. } = event {
                    all_bytes.extend_from_slice(bytes);
                }
            }
            if !all_bytes.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let text = String::from_utf8_lossy(&all_bytes);

        // The partial line should be present with a synthetic newline.
        assert!(
            text.contains("partial_no_newline"),
            "expected partial line to be flushed on disconnect, got: {text}"
        );
        assert!(
            all_bytes.ends_with(b"\n"),
            "expected synthetic newline at end of flushed data"
        );
    }
}
