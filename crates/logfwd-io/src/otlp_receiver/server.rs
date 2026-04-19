use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
#[cfg(any(feature = "otlp-research", test))]
use bytes::Bytes;
use logfwd_types::diagnostics::{ComponentHealth, ComponentStats};

use crate::InputError;
use crate::receiver_http::{parse_content_length, parse_content_type, read_limited_body};

use super::decode::{decode_otlp_json, decode_otlp_protobuf, decompress_gzip, decompress_zstd};
#[cfg(any(feature = "otlp-research", test))]
use super::projection::ProjectionError;
use super::{OtlpProtobufDecodeMode, OtlpServerState, ReceiverPayload};

pub(super) fn record_error(stats: Option<&Arc<ComponentStats>>) {
    if let Some(stats) = stats {
        stats.inc_errors();
    }
}

fn record_parse_error(stats: Option<&Arc<ComponentStats>>) {
    if let Some(stats) = stats {
        stats.inc_parse_errors(1);
    }
}

pub(super) async fn handle_otlp_request(
    State(state): State<Arc<OtlpServerState>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let max_body = state.max_message_size_bytes;
    let content_length = parse_content_length(&headers);
    if content_length.is_some_and(|body_len| body_len > max_body as u64) {
        record_error(state.stats.as_ref());
        return (StatusCode::PAYLOAD_TOO_LARGE, "payload too large").into_response();
    }

    let content_encoding = match parse_content_encoding(&headers) {
        Ok(content_encoding) => content_encoding,
        Err(status) => {
            record_error(state.stats.as_ref());
            return (status, "invalid content-encoding header").into_response();
        }
    };

    let mut body = match read_limited_body(body, max_body, content_length).await {
        Ok(body) => body,
        Err(status) => {
            record_error(state.stats.as_ref());
            let message = if status == StatusCode::PAYLOAD_TOO_LARGE {
                "payload too large"
            } else {
                "read error"
            };
            return (status, message).into_response();
        }
    };

    let accounted_bytes = body.len() as u64;
    body = match content_encoding.as_deref() {
        Some("zstd") => match decompress_zstd(&body, max_body) {
            Ok(body) => body,
            Err(InputError::Receiver(msg)) => {
                record_error(state.stats.as_ref());
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            Err(InputError::Io(e)) if e.kind() == io::ErrorKind::InvalidData => {
                record_error(state.stats.as_ref());
                return (StatusCode::PAYLOAD_TOO_LARGE, e.to_string()).into_response();
            }
            Err(_) => {
                record_error(state.stats.as_ref());
                return (StatusCode::BAD_REQUEST, "zstd decompression failed").into_response();
            }
        },
        Some("gzip") => match decompress_gzip(&body, max_body) {
            Ok(body) => body,
            Err(InputError::Receiver(msg)) => {
                record_error(state.stats.as_ref());
                return (StatusCode::BAD_REQUEST, msg).into_response();
            }
            Err(InputError::Io(e)) if e.kind() == io::ErrorKind::InvalidData => {
                record_error(state.stats.as_ref());
                return (StatusCode::PAYLOAD_TOO_LARGE, e.to_string()).into_response();
            }
            Err(_) => {
                record_error(state.stats.as_ref());
                return (StatusCode::BAD_REQUEST, "gzip decompression failed").into_response();
            }
        },
        None | Some("identity") => body,
        Some(other) => {
            record_error(state.stats.as_ref());
            return (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("unsupported content-encoding: {other}"),
            )
                .into_response();
        }
    };

    let content_type = match parse_content_type(&headers) {
        Ok(content_type) => content_type,
        Err(status) => {
            record_error(state.stats.as_ref());
            return (status, "invalid content-type header").into_response();
        }
    };
    let is_json = matches!(content_type.as_deref(), Some("application/json"));

    let batch = if is_json {
        decode_otlp_json(&body, &state.resource_prefix)
    } else {
        decode_otlp_protobuf_request(body, &state).await
    };
    let batch = match batch {
        Ok(batch) => batch,
        Err(msg) => {
            record_parse_error(state.stats.as_ref());
            return (StatusCode::BAD_REQUEST, msg.to_string()).into_response();
        }
    };

    if batch.num_rows() == 0 {
        state
            .health
            .store(ComponentHealth::Healthy.as_repr(), Ordering::Relaxed);
        return (StatusCode::OK, [(CONTENT_TYPE, "application/json")], "{}").into_response();
    }

    let payload = ReceiverPayload {
        batch,
        accounted_bytes,
    };

    let send_result = state.tx.try_send(payload);

    match send_result {
        Ok(()) => {
            state
                .health
                .store(ComponentHealth::Healthy.as_repr(), Ordering::Relaxed);
            (StatusCode::OK, [(CONTENT_TYPE, "application/json")], "{}").into_response()
        }
        Err(mpsc::TrySendError::Full(_)) => {
            state
                .health
                .store(ComponentHealth::Degraded.as_repr(), Ordering::Relaxed);
            (
                StatusCode::TOO_MANY_REQUESTS,
                "too many requests: pipeline backpressure",
            )
                .into_response()
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
            record_error(state.stats.as_ref());
            if state.is_running.load(Ordering::Relaxed) {
                state
                    .health
                    .store(ComponentHealth::Failed.as_repr(), Ordering::Relaxed);
            }
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "service unavailable: pipeline disconnected",
            )
                .into_response()
        }
    }
}

async fn decode_otlp_protobuf_request(
    body: Vec<u8>,
    state: &OtlpServerState,
) -> Result<arrow::record_batch::RecordBatch, InputError> {
    match state.protobuf_decode_mode {
        OtlpProtobufDecodeMode::Prost => decode_otlp_protobuf(&body, &state.resource_prefix),
        #[cfg(any(feature = "otlp-research", test))]
        OtlpProtobufDecodeMode::ProjectedFallback => {
            decode_otlp_protobuf_projected_fallback(Bytes::from(body), state).await
        }
        #[cfg(any(feature = "otlp-research", test))]
        OtlpProtobufDecodeMode::ProjectedOnly => {
            decode_otlp_protobuf_projected_only(Bytes::from(body), state).await
        }
    }
}

#[cfg(any(feature = "otlp-research", test))]
async fn decode_otlp_protobuf_projected_fallback(
    body: Bytes,
    state: &OtlpServerState,
) -> Result<arrow::record_batch::RecordBatch, InputError> {
    match decode_with_reusable_projected_decoder(body.clone(), state).await {
        Ok(batch) => {
            record_projected_success(state.stats.as_ref());
            Ok(batch)
        }
        Err(ProjectionError::Unsupported(_)) => {
            record_projected_fallback(state.stats.as_ref());
            decode_otlp_protobuf(&body, &state.resource_prefix)
        }
        Err(err) => {
            record_projection_invalid(state.stats.as_ref());
            Err(err.into_input_error())
        }
    }
}

#[cfg(any(feature = "otlp-research", test))]
async fn decode_otlp_protobuf_projected_only(
    body: Bytes,
    state: &OtlpServerState,
) -> Result<arrow::record_batch::RecordBatch, InputError> {
    match decode_with_reusable_projected_decoder(body, state).await {
        Ok(batch) => {
            record_projected_success(state.stats.as_ref());
            Ok(batch)
        }
        Err(ProjectionError::Unsupported(err)) => {
            Err(ProjectionError::Unsupported(err).into_input_error())
        }
        Err(err) => {
            record_projection_invalid(state.stats.as_ref());
            Err(err.into_input_error())
        }
    }
}

#[cfg(any(feature = "otlp-research", test))]
async fn decode_with_reusable_projected_decoder(
    body: Bytes,
    state: &OtlpServerState,
) -> Result<arrow::record_batch::RecordBatch, ProjectionError> {
    let decoder = state
        .projected_decoder
        .as_ref()
        .ok_or_else(|| ProjectionError::Batch("projected decoder not initialized".to_string()))?;
    let mut decoder = decoder.lock().await;
    decoder.try_decode_view_bytes(body)
}

#[cfg(any(feature = "otlp-research", test))]
fn record_projected_success(stats: Option<&Arc<ComponentStats>>) {
    if let Some(stats) = stats {
        stats.inc_otlp_projected_success();
    }
}

#[cfg(any(feature = "otlp-research", test))]
fn record_projected_fallback(stats: Option<&Arc<ComponentStats>>) {
    if let Some(stats) = stats {
        stats.inc_otlp_projected_fallback();
    }
}

#[cfg(any(feature = "otlp-research", test))]
fn record_projection_invalid(stats: Option<&Arc<ComponentStats>>) {
    if let Some(stats) = stats {
        stats.inc_otlp_projection_invalid();
    }
}

fn parse_content_encoding(headers: &HeaderMap) -> Result<Option<String>, StatusCode> {
    let Some(value) = headers.get(CONTENT_ENCODING) else {
        return Ok(None);
    };
    let parsed = value.to_str().map_err(|_| StatusCode::BAD_REQUEST)?.trim();
    if parsed.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(Some(parsed.to_ascii_lowercase()))
}
