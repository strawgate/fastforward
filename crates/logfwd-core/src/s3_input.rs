//! S3 input source: polls an S3 bucket for new objects and returns their
//! contents as `InputEvent::Data` events.
//!
//! Objects are identified by `key#etag`; once ingested they are not re-read
//! unless overwritten (ETag change).  Pagination is handled automatically.

use std::collections::HashSet;
use std::io;

use aws_sdk_s3::Client;
use aws_sdk_s3::config::Region;

use crate::input::{InputEvent, InputSource};

// ---------------------------------------------------------------------------
// Public struct
// ---------------------------------------------------------------------------

/// An input source that reads objects from an S3 (or S3-compatible) bucket.
///
/// On each call to [`poll`] the receiver lists all objects under the
/// configured prefix and downloads any that have not been seen before.  A
/// tokio current-thread runtime is embedded so that the async AWS SDK calls
/// can be driven from the synchronous `poll` interface.
pub struct S3Input {
    name: String,
    client: Client,
    bucket: String,
    /// Optional key prefix to filter listed objects.
    prefix: Option<String>,
    /// Tracks `"<key>#<etag>"` strings for objects already ingested.
    seen: HashSet<String>,
    runtime: tokio::runtime::Runtime,
}

impl S3Input {
    /// Create a new `S3Input`.
    ///
    /// - `bucket`       – S3 bucket name.
    /// - `prefix`       – Optional key prefix (maps to the `path` config field).
    /// - `region`       – AWS region; falls back to environment / instance metadata if `None`.
    /// - `endpoint_url` – Custom endpoint for S3-compatible stores (MinIO, etc.).
    pub fn new(
        name: String,
        bucket: String,
        prefix: Option<String>,
        region: Option<String>,
        endpoint_url: Option<String>,
    ) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(io::Error::other)?;

        let client = runtime.block_on(build_client(region, endpoint_url));

        Ok(S3Input {
            name,
            client,
            bucket,
            prefix,
            seen: HashSet::new(),
            runtime,
        })
    }
}

// ---------------------------------------------------------------------------
// InputSource impl
// ---------------------------------------------------------------------------

impl InputSource for S3Input {
    /// List the bucket, download any new objects, and return their bytes.
    fn poll(&mut self) -> io::Result<Vec<InputEvent>> {
        // Collect (seen_key, bytes) for new objects.  We resolve all borrows
        // inside the block so we can mutate `self.seen` afterwards.
        let new_objects: Vec<(String, Vec<u8>)> = {
            let rt = &self.runtime;
            let client = &self.client;
            let bucket = self.bucket.as_str();
            let prefix = self.prefix.as_deref();
            let seen = &self.seen;

            rt.block_on(fetch_new_objects(client, bucket, prefix, seen))?
        };

        let mut events = Vec::with_capacity(new_objects.len());
        for (seen_key, bytes) in new_objects {
            self.seen.insert(seen_key);
            events.push(InputEvent::Data { bytes });
        }

        Ok(events)
    }

    fn name(&self) -> &str {
        &self.name
    }
}

// ---------------------------------------------------------------------------
// Async helpers (kept free-standing so borrow checker is happy)
// ---------------------------------------------------------------------------

async fn build_client(region: Option<String>, endpoint_url: Option<String>) -> Client {
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(r) = region {
        loader = loader.region(Region::new(r));
    }
    let sdk_config = loader.load().await;

    let mut s3_builder = aws_sdk_s3::config::Builder::from(&sdk_config);
    if let Some(ep) = endpoint_url {
        s3_builder = s3_builder.endpoint_url(ep).force_path_style(true);
    }

    Client::from_conf(s3_builder.build())
}

async fn fetch_new_objects(
    client: &Client,
    bucket: &str,
    prefix: Option<&str>,
    seen: &HashSet<String>,
) -> io::Result<Vec<(String, Vec<u8>)>> {
    let mut result: Vec<(String, Vec<u8>)> = Vec::new();
    let mut continuation_token: Option<String> = None;

    loop {
        let mut req = client.list_objects_v2().bucket(bucket);
        if let Some(p) = prefix {
            req = req.prefix(p);
        }
        if let Some(token) = continuation_token.take() {
            req = req.continuation_token(token);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| io::Error::other(format!("S3 list error: {e}")))?;

        for obj in resp.contents() {
            let key = match obj.key() {
                Some(k) => k,
                None => continue,
            };
            let etag = obj.e_tag().unwrap_or("");
            let seen_key = format!("{key}#{etag}");

            if seen.contains(&seen_key) {
                continue;
            }

            let get_resp = client
                .get_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| io::Error::other(format!("S3 get '{key}' error: {e}")))?;

            let bytes = get_resp
                .body
                .collect()
                .await
                .map_err(|e| io::Error::other(format!("S3 read '{key}' error: {e}")))?
                .into_bytes()
                .to_vec();

            result.push((seen_key, bytes));
        }

        if resp.is_truncated().unwrap_or(false) {
            continuation_token = resp.next_continuation_token().map(str::to_string);
        } else {
            break;
        }
    }

    Ok(result)
}
