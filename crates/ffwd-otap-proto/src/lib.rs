// Generated protobuf code — prost does not derive Eq.
#![allow(clippy::derive_partial_eq_without_eq)]
//! Generated protobuf message types for OTAP transport-edge messages.
//!
//! This crate intentionally keeps codegen at a small protocol boundary so the
//! receiver and sink can share one schema without spreading generation through
//! the rest of the repo.

pub mod otap {
    include!(concat!(env!("OUT_DIR"), "/otap.rs"));
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use prost::Message as _; // Required for encode_to_vec and decode.
    use super::otap::{
        ArrowPayload, ArrowPayloadType, BatchArrowRecords, BatchStatus, StatusCode,
    };

    fn roundtrip<T: prost::Message + Default + PartialEq>(msg: &T) -> bool {
        let encoded = msg.encode_to_vec();
        let decoded = T::decode(encoded.as_slice()).ok();
        decoded.as_ref() == Some(msg)
    }

    #[test]
    fn batch_arrow_records_roundtrip() {
        let batch = BatchArrowRecords {
            batch_id: 42,
            arrow_payloads: vec![
                ArrowPayload {
                    schema_id: "schema-1".into(),
                    r#type: ArrowPayloadType::Logs.into(),
                    record: b"test record data".to_vec(),
                },
                ArrowPayload {
                    schema_id: "schema-2".into(),
                    r#type: ArrowPayloadType::LogAttrs.into(),
                    record: b"another record".to_vec(),
                },
            ],
            headers: b"header bytes".to_vec(),
        };
        assert!(roundtrip(&batch));
    }

    #[test]
    fn arrow_payload_type_enum_variants() {
        for variant in [
            ArrowPayloadType::Logs,
            ArrowPayloadType::LogAttrs,
            ArrowPayloadType::ResourceAttrs,
            ArrowPayloadType::ScopeAttrs,
        ] {
            let payload = ArrowPayload {
                schema_id: "test".into(),
                r#type: variant.into(),
                record: vec![],
            };
            assert!(roundtrip(&payload), "roundtrip failed for {variant:?}");
        }
    }

    #[test]
    fn status_code_enum_variants() {
        for variant in [StatusCode::Ok, StatusCode::Unavailable, StatusCode::InvalidArgument] {
            let status = BatchStatus {
                batch_id: 1,
                status_code: variant.into(),
                status_message: "test message".into(),
            };
            assert!(roundtrip(&status), "roundtrip failed for {variant:?}");
        }
    }

    #[test]
    fn batch_status_roundtrip() {
        let status = BatchStatus {
            batch_id: 99,
            status_code: StatusCode::Unavailable.into(),
            status_message: "service unavailable".into(),
        };
        assert!(roundtrip(&status));
    }

    #[test]
    fn arrow_payload_empty_record_roundtrip() {
        let payload = ArrowPayload {
            schema_id: "empty".into(),
            r#type: ArrowPayloadType::Logs.into(),
            record: vec![],
        };
        assert!(roundtrip(&payload));
    }
}
