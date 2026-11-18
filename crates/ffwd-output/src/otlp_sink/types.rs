//! Core types and constants for the OTLP sink.

use std::io;
use std::sync::Arc;

use arrow::array::{Array, PrimitiveArray};
use arrow::datatypes::{Int64Type, UInt32Type, UInt64Type};

use ffwd_config::{OtlpProtocol, OtlpSignal};
use ffwd_types::diagnostics::ComponentStats;
use zstd::bulk::Compressor as ZstdCompressor;

use super::Compression;

// ---------------------------------------------------------------------------
// InstrumentationScope constants
// ---------------------------------------------------------------------------

/// Name emitted in the OTLP `InstrumentationScope.name` field of every `ScopeLogs`.
pub(super) const SCOPE_NAME: &[u8] = b"ff";
/// Version emitted in the OTLP `InstrumentationScope.version` field (from Cargo.toml).
pub(super) const SCOPE_VERSION: &[u8] = env!("CARGO_PKG_VERSION").as_bytes();
/// Default gRPC outbound message size guardrail.
pub(super) const DEFAULT_GRPC_MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// OtlpSink
// ---------------------------------------------------------------------------

/// Sends OTLP protobuf LogRecords over gRPC or HTTP.
pub struct OtlpSink {
    pub(super) name: String,
    pub(super) endpoint: String,
    /// Endpoint used when a batch is sent as OTLP metrics.
    pub(super) metrics_endpoint: String,
    pub(super) signal: OtlpSignal,
    pub(super) protocol: OtlpProtocol,
    pub(super) compression: Compression,
    pub(super) headers: Vec<(String, String)>,
    pub(super) message_field: String,
    pub(crate) encoder_buf: Vec<u8>,
    pub(super) compress_buf: Vec<u8>,
    pub(super) grpc_buf: Vec<u8>,
    pub(super) compressor: Option<ZstdCompressor<'static>>,
    pub(super) client: reqwest::Client,
    pub(super) stats: Arc<ComponentStats>,
}

impl OtlpSink {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        endpoint: String,
        protocol: OtlpProtocol,
        compression: Compression,
        headers: Vec<(String, String)>,
        client: reqwest::Client,
        stats: Arc<ComponentStats>,
    ) -> io::Result<Self> {
        use ffwd_types::field_names;

        let metrics_endpoint = metrics_endpoint_for(&endpoint, protocol);
        // For gRPC, ensure the endpoint has the correct service path.
        // Users typically configure just the host:port (e.g., "http://collector:4317").
        // The gRPC spec requires the full path: /package.Service/Method (#1059)
        let endpoint = if protocol == OtlpProtocol::Grpc {
            let trimmed = endpoint.trim_end_matches('/');
            if trimmed.ends_with("/Export") || trimmed.ends_with("/LogsService/Export") {
                endpoint
            } else {
                format!("{trimmed}/opentelemetry.proto.collector.logs.v1.LogsService/Export")
            }
        } else {
            endpoint
        };
        let compressor = match compression {
            Compression::Zstd => Some(ZstdCompressor::new(1).map_err(io::Error::other)?),
            _ => None,
        };
        Ok(OtlpSink {
            name,
            endpoint,
            metrics_endpoint,
            signal: OtlpSignal::Auto,
            protocol,
            compression,
            headers,
            message_field: field_names::BODY.to_string(),
            encoder_buf: Vec::with_capacity(64 * 1024),
            compress_buf: Vec::with_capacity(64 * 1024),
            grpc_buf: Vec::with_capacity(64 * 1024),
            compressor,
            client,
            stats,
        })
    }

    /// Override the source column used for OTLP `LogRecord.body`.
    ///
    /// The default is the canonical `body` field. Use this builder when a
    /// pipeline stores the primary log message under a different string column.
    #[inline]
    #[must_use]
    pub fn with_message_field(mut self, message_field: String) -> Self {
        self.message_field = message_field;
        self
    }

    /// Choose which OTLP signal this sink sends. See [`OtlpSignal`].
    #[inline]
    #[must_use]
    pub fn with_signal(mut self, signal: OtlpSignal) -> Self {
        self.signal = signal;
        self
    }

    /// Returns the raw encoded OTLP protobuf payload produced by one of the
    /// encode methods on this sink.
    pub fn encoded_payload(&self) -> &[u8] {
        &self.encoder_buf
    }
}

const GRPC_LOGS_PATH: &str = "/opentelemetry.proto.collector.logs.v1.LogsService/Export";
const GRPC_METRICS_PATH: &str = "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export";

/// Derive the metrics endpoint from the configured (logs) endpoint.
///
/// gRPC: the service path is replaced with `MetricsService/Export`.
/// HTTP: a path ending in `/v1/logs` becomes `/v1/metrics`, keeping any
/// query string or fragment; any other URL is used as configured, so a dedicated metrics
/// output can point anywhere.
pub(super) fn metrics_endpoint_for(endpoint: &str, protocol: OtlpProtocol) -> String {
    if protocol == OtlpProtocol::Http {
        let (path, query) = endpoint
            .find(['?', '#'])
            .map_or((endpoint, ""), |i| endpoint.split_at(i));
        return match path.trim_end_matches('/').strip_suffix("/v1/logs") {
            Some(base) => format!("{base}/v1/metrics{query}"),
            None => endpoint.to_string(),
        };
    }
    let trimmed = endpoint.trim_end_matches('/');
    if trimmed.ends_with(GRPC_METRICS_PATH) {
        return trimmed.to_string();
    }
    let base = trimmed.strip_suffix(GRPC_LOGS_PATH).unwrap_or(trimmed);
    format!("{base}{GRPC_METRICS_PATH}")
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum ResourceValueRef<'a> {
    Str(&'a str),
    Bytes(&'a [u8]),
    Int(i64),
    Float(u64),
    Bool(bool),
}

/// Per-row resource key: one optional borrowed/scalar value per resource column.
pub(super) type ResourceKey<'a> = Vec<Option<ResourceValueRef<'a>>>;

/// Per-group scope key (name, version).
pub(super) type ScopeKey<'a> = (Option<&'a str>, Option<&'a str>);

/// Per-group state: resource key + scope key + byte-range spans of encoded log records.
pub(super) type ResourceGroup<'a> = (ResourceKey<'a>, ScopeKey<'a>, Vec<(usize, usize)>);

#[derive(Clone, Copy)]
pub(super) enum FlagsArray<'a> {
    Int64(&'a PrimitiveArray<Int64Type>),
    UInt32(&'a PrimitiveArray<UInt32Type>),
    UInt64(&'a PrimitiveArray<UInt64Type>),
}

impl FlagsArray<'_> {
    #[inline]
    pub(super) fn value_u32(self, row: usize) -> Option<u32> {
        match self {
            FlagsArray::Int64(arr) => (!arr.is_null(row))
                .then(|| arr.value(row))
                .and_then(|raw| u32::try_from(raw).ok()),
            FlagsArray::UInt32(arr) => (!arr.is_null(row)).then(|| arr.value(row)),
            FlagsArray::UInt64(arr) => (!arr.is_null(row))
                .then(|| arr.value(row))
                .and_then(|raw| u32::try_from(raw).ok()),
        }
    }
}
