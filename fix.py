with open("crates/logfwd-io/src/otap_receiver.rs", "r") as f:
    content = f.read()

content = content.replace('use axum::http::header::CONTENT_TYPE;', 'use axum::http::header::{CONTENT_TYPE, CONTENT_ENCODING};')

content = content.replace('''async fn handle_otap_request(
    State(state): State<Arc<OtapServerState>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let content_length = parse_content_length(&headers);''', '''async fn handle_otap_request(
    State(state): State<Arc<OtapServerState>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let content_encoding = match parse_content_encoding(&headers) {
        Ok(content_encoding) => content_encoding,
        Err(status) => return (status, "invalid content-encoding header").into_response(),
    };

    match parse_content_type(&headers) {
        Ok(Some(content_type)) => {
            if content_type != "application/x-protobuf" {
                return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported content-type").into_response();
            }
        }
        Ok(None) => {
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "missing content-type").into_response();
        }
        Err(status) => return (status, "invalid content-type header").into_response(),
    }

    let content_length = parse_content_length(&headers);''')

content = content.replace('''    let body = match read_limited_body(body, MAX_REQUEST_BODY_SIZE, content_length).await {
        Ok(body) => body,
        Err(status) => {
            let message = if status == StatusCode::PAYLOAD_TOO_LARGE {
                "payload too large"
            } else {
                "read error"
            };
            return (status, message).into_response();
        }
    };

    let batch_records = match decode_batch_arrow_records(&body) {''', '''    let body = match read_limited_body(body, MAX_REQUEST_BODY_SIZE, content_length).await {
        Ok(body) => body,
        Err(status) => {
            let message = if status == StatusCode::PAYLOAD_TOO_LARGE {
                "payload too large"
            } else {
                "read error"
            };
            return (status, message).into_response();
        }
    };

    let body = match content_encoding.as_deref() {
        Some("gzip") => match decompress_gzip(&body, MAX_REQUEST_BODY_SIZE) {
            Ok(decompressed) => decompressed,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "gzip decompression failed").into_response();
            }
        },
        Some(other) => {
            return (StatusCode::BAD_REQUEST, format!("unsupported content-encoding: {other}")).into_response();
        }
        None => body,
    };

    let batch_records = match decode_batch_arrow_records(&body) {''')

content = content.replace('''// ---------------------------------------------------------------------------
// Star schema assembly
// ---------------------------------------------------------------------------''', '''fn parse_content_encoding(headers: &HeaderMap) -> Result<Option<String>, StatusCode> {
    let Some(value) = headers.get(CONTENT_ENCODING) else {
        return Ok(None);
    };
    let parsed = value.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;
    let encoding = parsed.trim();
    if encoding.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if encoding.eq_ignore_ascii_case("identity") {
        return Ok(None);
    }
    Ok(Some(encoding.to_ascii_lowercase()))
}

fn decompress_gzip(body: &[u8], max_request_body_size: usize) -> Result<Vec<u8>, InputError> {
    let decoder = flate2::read::GzDecoder::new(body);
    read_decompressed_body(
        decoder,
        body.len(),
        max_request_body_size,
        "gzip decompression failed",
    )
}

fn read_decompressed_body(
    mut reader: impl std::io::Read,
    compressed_len: usize,
    max_request_body_size: usize,
    error_label: &str,
) -> Result<Vec<u8>, InputError> {
    use std::io::Read;
    let mut decompressed = Vec::with_capacity(compressed_len.min(max_request_body_size));
    match reader
        .by_ref()
        .take(max_request_body_size as u64 + 1)
        .read_to_end(&mut decompressed)
    {
        Ok(n) if n > max_request_body_size => Err(InputError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "payload too large",
        ))),
        Ok(_) => Ok(decompressed),
        Err(_) => Err(InputError::Receiver(error_label.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Star schema assembly
// ---------------------------------------------------------------------------''')

with open("crates/logfwd-io/src/otap_receiver.rs", "w") as f:
    f.write(content)
