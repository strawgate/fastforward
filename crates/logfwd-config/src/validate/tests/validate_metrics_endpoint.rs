#[cfg(test)]
mod validate_metrics_endpoint_tests {
    use crate::types::Config;

    #[test]
    fn metrics_endpoint_valid_url_accepted() {
        let yaml = "server:\n  metrics_endpoint: http://localhost:4318/v1/metrics\npipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n    outputs:\n      - type: stdout\n";
        Config::load_str(yaml).expect("valid metrics_endpoint should be accepted");
    }

    #[test]
    fn metrics_endpoint_invalid_url_rejected() {
        let yaml = "server:\n  metrics_endpoint: not-a-url\npipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n    outputs:\n      - type: stdout\n";
        let err = Config::load_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("metrics_endpoint"),
            "expected metrics_endpoint rejection: {msg}"
        );
    }

    #[test]
    fn metrics_endpoint_ftp_scheme_rejected() {
        let yaml = "server:\n  metrics_endpoint: ftp://localhost:21/metrics\npipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n    outputs:\n      - type: stdout\n";
        let err = Config::load_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("metrics_endpoint") && msg.contains("scheme"),
            "expected metrics_endpoint scheme rejection: {msg}"
        );
    }
}
