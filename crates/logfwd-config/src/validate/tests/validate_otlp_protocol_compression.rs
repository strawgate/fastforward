#[cfg(test)]
mod validate_otlp_protocol_compression_tests {
    use crate::types::Config;

    #[test]
    fn otlp_valid_protocol_accepted() {
        for proto in ["http", "grpc"] {
            let yaml = format!(
                "pipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n    outputs:\n      - type: otlp\n        endpoint: http://localhost:4317\n        protocol: {proto}\n"
            );
            Config::load_str(&yaml)
                .unwrap_or_else(|e| panic!("protocol '{proto}' should be accepted: {e}"));
        }
    }

    #[test]
    fn otlp_invalid_protocol_rejected() {
        let yaml = "pipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n    outputs:\n      - type: otlp\n        endpoint: http://localhost:4317\n        protocol: websocket\n";
        let err = Config::load_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("websocket") && msg.contains("http") && msg.contains("grpc"),
            "expected protocol rejection for 'websocket': {msg}"
        );
    }

    #[test]
    fn otlp_valid_compression_accepted() {
        for comp in ["zstd", "gzip", "none"] {
            let yaml = format!(
                "pipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n    outputs:\n      - type: otlp\n        endpoint: http://localhost:4317\n        compression: {comp}\n"
            );
            Config::load_str(&yaml).unwrap_or_else(|e| {
                panic!("compression '{comp}' should be accepted for otlp: {e}")
            });
        }
    }

    #[test]
    fn otlp_invalid_compression_rejected() {
        let yaml = "pipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n    outputs:\n      - type: otlp\n        endpoint: http://localhost:4317\n        compression: lz4\n";
        let err = Config::load_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("lz4") && msg.contains("zstd") && msg.contains("gzip"),
            "expected compression rejection for 'lz4': {msg}"
        );
    }

    #[test]
    fn elasticsearch_invalid_compression_rejected() {
        let yaml = "pipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n    outputs:\n      - type: elasticsearch\n        endpoint: http://localhost:9200\n        compression: zstd\n";
        let err = Config::load_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("compression") && msg.contains("zstd"),
            "expected compression rejection for 'zstd' on elasticsearch: {msg}"
        );
    }

    #[test]
    fn elasticsearch_valid_compression_accepted() {
        for comp in ["gzip", "none"] {
            let yaml = format!(
                "pipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n    outputs:\n      - type: elasticsearch\n        endpoint: http://localhost:9200\n        compression: {comp}\n"
            );
            Config::load_str(&yaml).unwrap_or_else(|e| {
                panic!("compression '{comp}' should be accepted for elasticsearch: {e}")
            });
        }
    }
}
