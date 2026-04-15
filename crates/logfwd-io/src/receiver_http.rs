use std::io;

use axum::body::Body;
use axum::http::{
    HeaderMap, StatusCode,
    header::{CONTENT_LENGTH, CONTENT_TYPE},
};
use flate2::read::GzDecoder;
use http_body_util::BodyExt as _;

use crate::InputError;
/// Maximum request body size shared by all HTTP receivers: 10 MB.
pub(crate) const MAX_REQUEST_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Parses the Content-Length header value.
///
/// Returns `None` when the header is missing, non-UTF-8, or not a valid `u64`.
pub(crate) fn parse_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

pub(crate) fn parse_content_type(headers: &HeaderMap) -> Result<Option<String>, StatusCode> {
    let Some(value) = headers.get(CONTENT_TYPE) else {
        return Ok(None);
    };
    let raw = value
        .to_str()
        .map_err(|_| StatusCode::UNSUPPORTED_MEDIA_TYPE)?;
    let media_type = raw.split(';').next().unwrap_or_default().trim();
    if media_type.is_empty() {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    Ok(Some(media_type.to_ascii_lowercase()))
}

pub(crate) async fn read_limited_body(
    body: Body,
    max_body_size: usize,
    content_length_hint: Option<u64>,
) -> Result<Vec<u8>, StatusCode> {
    if content_length_hint.is_some_and(|hint| hint > max_body_size as u64) {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    let mut body = body;
    let mut out = Vec::with_capacity(
        content_length_hint
            .map(|hint| hint.min(max_body_size as u64) as usize)
            .unwrap_or_default(),
    );

    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
        let Ok(chunk) = frame.into_data() else {
            continue;
        };
        if out.len().saturating_add(chunk.len()) > max_body_size {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Decompress gzip-encoded body with size limit.
///
/// Returns an error if the decompressed payload exceeds `max_request_body_size`.
pub(crate) fn decompress_gzip(
    body: &[u8],
    max_request_body_size: usize,
) -> Result<Vec<u8>, InputError> {
    let decoder = GzDecoder::new(body);
    read_decompressed_body(
        decoder,
        body.len(),
        max_request_body_size,
        "gzip decompression failed",
    )
}

/// Decompress zstd-encoded body with size limit.
///
/// Returns an error if the decompressed payload exceeds `max_request_body_size`.
pub(crate) fn decompress_zstd(
    body: &[u8],
    max_request_body_size: usize,
) -> Result<Vec<u8>, InputError> {
    let decoder = zstd::Decoder::new(body)
        .map_err(|_| InputError::Receiver("zstd decompression failed".to_string()))?;
    read_decompressed_body(
        decoder,
        body.len(),
        max_request_body_size,
        "zstd decompression failed",
    )
}

/// Read from a decompressor with a bounded take to enforce size limits.
///
/// Returns an error if the decompressed size exceeds `max_request_body_size`.
fn read_decompressed_body(
    reader: impl io::Read,
    compressed_len: usize,
    max_request_body_size: usize,
    error_label: &str,
) -> Result<Vec<u8>, InputError> {
    use io::Read;
    let mut decompressed = Vec::with_capacity(compressed_len.min(max_request_body_size));
    match reader
        .take(max_request_body_size as u64 + 1)
        .read_to_end(&mut decompressed)
    {
        Ok(n) if n > max_request_body_size => Err(InputError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload too large",
        ))),
        Ok(_) => Ok(decompressed),
        Err(_) => Err(InputError::Receiver(error_label.to_string())),
    }
}
