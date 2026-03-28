//! HTTP sender: POSTs batches of log lines to a VictoriaLogs-compatible
//! /insert/jsonline endpoint, matching the VM benchmark format.

use std::io;
use std::time::{Duration, Instant};

/// Configuration for the HTTP sender.
#[derive(Clone, Debug)]
pub struct SenderConfig {
    /// Full URL of the insert endpoint, e.g. "http://localhost:8080/insert/jsonline"
    pub endpoint: String,
    /// Value for the VL-Extra-Fields header (e.g. "collector=logfwd")
    pub extra_fields: String,
    /// Value for the VL-Stream-Fields header
    pub stream_fields: String,
    /// Whether to compress the request body with zstd
    pub compress: bool,
    /// HTTP timeout
    pub timeout: Duration,
}

impl Default for SenderConfig {
    fn default() -> Self {
        SenderConfig {
            endpoint: "http://localhost:8080/insert/jsonline".into(),
            extra_fields: "collector=logfwd".into(),
            stream_fields: "collector".into(),
            compress: false,
            timeout: Duration::from_secs(10),
        }
    }
}

/// Stats for a single send operation.
#[derive(Clone, Debug, Default)]
pub struct SendStats {
    pub bytes_sent: usize,
    pub send_ns: u64,
    pub status: u16,
}

/// Blocking HTTP sender. Sends batches of raw bytes (newline-delimited JSON)
/// to the VictoriaLogs insert API.
pub struct HttpSender {
    config: SenderConfig,
    agent: ureq::Agent,
}

impl HttpSender {
    pub fn new(config: SenderConfig) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(config.timeout))
            .build()
            .new_agent();
        HttpSender { config, agent }
    }

    /// Send a batch of raw bytes. The bytes should be newline-delimited JSON.
    /// Returns send stats on success.
    pub fn send(&self, body: &[u8]) -> io::Result<SendStats> {
        if body.is_empty() {
            return Ok(SendStats::default());
        }

        let start = Instant::now();

        let response = self.agent
            .post(&self.config.endpoint)
            .header("Content-Type", "application/stream+json")
            .header("VL-Extra-Fields", &self.config.extra_fields)
            .header("VL-Stream-Fields", &self.config.stream_fields)
            .send(body)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        let status = response.status().as_u16();
        let send_ns = start.elapsed().as_nanos() as u64;

        if status >= 400 {
            let body_text = response.into_body().read_to_string()
                .unwrap_or_else(|_| "failed to read response body".into());
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("HTTP {status}: {body_text}"),
            ));
        }

        Ok(SendStats {
            bytes_sent: body.len(),
            send_ns,
            status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sender_config_default() {
        let config = SenderConfig::default();
        assert!(config.endpoint.contains("jsonline"));
        assert!(config.extra_fields.contains("logfwd"));
    }
}
