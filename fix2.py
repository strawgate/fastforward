with open("crates/logfwd-io/src/otap_receiver.rs", "r") as f:
    content = f.read()

content = content.replace('use crate::receiver_http::{MAX_REQUEST_BODY_SIZE, parse_content_length, read_limited_body};', 'use crate::receiver_http::{MAX_REQUEST_BODY_SIZE, parse_content_length, read_limited_body, parse_content_type};')

with open("crates/logfwd-io/src/otap_receiver.rs", "w") as f:
    f.write(content)
