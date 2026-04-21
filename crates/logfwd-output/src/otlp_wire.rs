use std::io;

use logfwd_core::otlp::{
    self, bytes_field_size, encode_bytes_field, encode_fixed64, encode_tag, encode_varint,
    encode_varint_field, varint_len,
};

/// Encode a KeyValue with string AnyValue (`AnyValue.string_value`).
///
/// Use for encoding resource and log record attributes.
/// Used to represent OpenTelemetry `AnyValue` strings in KeyValue lists
/// (e.g. `LOG_RECORD_ATTRIBUTES` for LogRecord, `RESOURCE_ATTRIBUTES` for Resource).
/// KeyValue: { key (string), value (AnyValue { string_value }) }
pub(crate) fn encode_key_value_string(
    buf: &mut Vec<u8>,
    field_number: u32,
    key: &[u8],
    value: &[u8],
) {
    let anyvalue_inner = bytes_field_size(otlp::ANY_VALUE_STRING_VALUE, value.len());
    let kv_inner = bytes_field_size(otlp::KEY_VALUE_KEY, key.len())
        + bytes_field_size(otlp::KEY_VALUE_VALUE, anyvalue_inner);
    encode_tag(buf, field_number, otlp::WIRE_TYPE_LEN);
    encode_varint(buf, kv_inner as u64);
    encode_bytes_field(buf, otlp::KEY_VALUE_KEY, key);
    encode_tag(buf, otlp::KEY_VALUE_VALUE, otlp::WIRE_TYPE_LEN);
    encode_varint(buf, anyvalue_inner as u64);
    encode_bytes_field(buf, otlp::ANY_VALUE_STRING_VALUE, value);
}

/// Encode a KeyValue with bytes AnyValue (`AnyValue.bytes_value`).
pub(crate) fn encode_key_value_bytes(
    buf: &mut Vec<u8>,
    field_number: u32,
    key: &[u8],
    value: &[u8],
) {
    let anyvalue_inner = bytes_field_size(otlp::ANY_VALUE_BYTES_VALUE, value.len());
    let kv_inner = bytes_field_size(otlp::KEY_VALUE_KEY, key.len())
        + bytes_field_size(otlp::KEY_VALUE_VALUE, anyvalue_inner);
    encode_tag(buf, field_number, otlp::WIRE_TYPE_LEN);
    encode_varint(buf, kv_inner as u64);
    encode_bytes_field(buf, otlp::KEY_VALUE_KEY, key);
    encode_tag(buf, otlp::KEY_VALUE_VALUE, otlp::WIRE_TYPE_LEN);
    encode_varint(buf, anyvalue_inner as u64);
    encode_bytes_field(buf, otlp::ANY_VALUE_BYTES_VALUE, value);
}

/// Encode a KeyValue with int AnyValue (`AnyValue.int_value`).
pub(crate) fn encode_key_value_int(buf: &mut Vec<u8>, field_number: u32, key: &[u8], value: i64) {
    let anyvalue_inner = 1 + varint_len(value as u64); // tag(1 byte) + varint
    let kv_inner = bytes_field_size(otlp::KEY_VALUE_KEY, key.len())
        + bytes_field_size(otlp::KEY_VALUE_VALUE, anyvalue_inner);
    encode_tag(buf, field_number, otlp::WIRE_TYPE_LEN);
    encode_varint(buf, kv_inner as u64);
    encode_bytes_field(buf, otlp::KEY_VALUE_KEY, key);
    encode_tag(buf, otlp::KEY_VALUE_VALUE, otlp::WIRE_TYPE_LEN);
    encode_varint(buf, anyvalue_inner as u64);
    encode_varint_field(buf, otlp::ANY_VALUE_INT_VALUE, value as u64);
}

/// Encode a KeyValue with double AnyValue (`AnyValue.double_value`).
pub(crate) fn encode_key_value_double(
    buf: &mut Vec<u8>,
    field_number: u32,
    key: &[u8],
    value: f64,
) {
    let anyvalue_inner = 1 + 8; // tag(1 byte) + fixed64
    let kv_inner = bytes_field_size(otlp::KEY_VALUE_KEY, key.len())
        + bytes_field_size(otlp::KEY_VALUE_VALUE, anyvalue_inner);
    encode_tag(buf, field_number, otlp::WIRE_TYPE_LEN);
    encode_varint(buf, kv_inner as u64);
    encode_bytes_field(buf, otlp::KEY_VALUE_KEY, key);
    encode_tag(buf, otlp::KEY_VALUE_VALUE, otlp::WIRE_TYPE_LEN);
    encode_varint(buf, anyvalue_inner as u64);
    encode_fixed64(buf, otlp::ANY_VALUE_DOUBLE_VALUE, value.to_bits());
}

/// Encode a KeyValue with boolean AnyValue (`AnyValue.bool_value`).
pub(crate) fn encode_key_value_bool(buf: &mut Vec<u8>, field_number: u32, key: &[u8], value: bool) {
    let anyvalue_inner = 1 + 1; // tag(1 byte) + varint(1 byte)
    let kv_inner = bytes_field_size(otlp::KEY_VALUE_KEY, key.len())
        + bytes_field_size(otlp::KEY_VALUE_VALUE, anyvalue_inner);
    encode_tag(buf, field_number, otlp::WIRE_TYPE_LEN);
    encode_varint(buf, kv_inner as u64);
    encode_bytes_field(buf, otlp::KEY_VALUE_KEY, key);
    encode_tag(buf, otlp::KEY_VALUE_VALUE, otlp::WIRE_TYPE_LEN);
    encode_varint(buf, anyvalue_inner as u64);
    encode_varint_field(buf, otlp::ANY_VALUE_BOOL_VALUE, u64::from(value));
}

/// Write a gRPC length-prefixed message frame into `buf`.
///
/// gRPC wire format (per the [gRPC over HTTP/2 specification](https://grpc.io/docs/what-is-grpc/core-concepts/)):
/// ```text
/// [1 byte: compressed flag (0 = not compressed, 1 = compressed)]
/// [4 bytes: big-endian message length]
/// [N bytes: protobuf message]
/// ```
pub(crate) fn write_grpc_frame(
    buf: &mut Vec<u8>,
    payload: &[u8],
    compressed: bool,
) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "gRPC message payload must be < 4 GiB",
        )
    })?;
    buf.clear();
    buf.push(u8::from(compressed));
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_frame_prepends_five_byte_header() {
        let mut framed = Vec::new();
        let payload: Vec<u8> = vec![0x01, 0x02, 0x03];
        write_grpc_frame(&mut framed, &payload, false).unwrap();
        assert_eq!(framed.len(), 8);
        assert_eq!(framed[0], 0);
        assert_eq!(&framed[1..5], &[0, 0, 0, 3]);
        assert_eq!(&framed[5..8], &[0x01, 0x02, 0x03]);
    }

    #[test]
    fn grpc_frame_compressed_flag() {
        let mut framed = Vec::new();
        write_grpc_frame(&mut framed, &[], true).unwrap();
        assert_eq!(framed[0], 1);
    }
}
