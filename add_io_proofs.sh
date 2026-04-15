#!/bin/bash

cat << 'INNER_EOF' >> crates/logfwd-io/src/otlp_receiver.rs

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    #[kani::unwind(21)] // max digits is ~20 for u64/i64
    fn verify_write_i64_ascii_only() {
        let mut buf = Vec::new();
        let n: i64 = kani::any();
        write_i64_to_buf(&mut buf, n);
        for &b in &buf {
            assert!(b.is_ascii());
        }
    }

    #[kani::proof]
    #[kani::unwind(1)]
    fn verify_write_f64_ascii_only() {
        let mut buf = Vec::new();
        let n: f64 = kani::any();
        write_f64_to_buf(&mut buf, n);
        for &b in &buf {
            assert!(b.is_ascii());
        }
    }

    #[kani::proof]
    #[kani::unwind(9)]
    fn verify_hex_encode_valid() {
        let mut buf = Vec::new();
        let len: usize = kani::any();
        kani::assume(len <= 8);
        let bytes: [u8; 8] = kani::any();
        write_hex_to_buf(&mut buf, &bytes[..len]);
        assert_eq!(buf.len(), len * 2);
        for &b in &buf {
            assert!(b.is_ascii_hexdigit());
        }
    }

    #[kani::proof]
    #[kani::unwind(9)]
    fn verify_json_escaping_ascii_only() {
        let mut buf = Vec::new();
        let len: usize = kani::any();
        kani::assume(len <= 8);
        let mut bytes = [0u8; 8];
        for i in 0..len {
            bytes[i] = kani::any();
            kani::assume(bytes[i].is_ascii());
        }
        let s = std::str::from_utf8(&bytes[..len]).unwrap();
        write_json_escaped_string_contents(&mut buf, s);
        for &b in &buf {
            assert!(b.is_ascii());
        }
    }
}
INNER_EOF
