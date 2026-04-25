#[cfg(test)]
mod validate_otlp_options_tests {
    use crate::types::Config;

    #[test]
    fn otlp_accepts_new_options() {
        let yaml = r"
pipelines:
  test:
    inputs:
      - type: file
        path: /tmp/test.log
    outputs:
      - type: otlp
        endpoint: http://localhost:4317
        retry_attempts: 3
        retry_initial_backoff_ms: 100
        retry_max_backoff_ms: 1000
        request_timeout_ms: 5000
        batch_size: 2048
        batch_timeout_ms: 1000
        headers:
          X-Custom: value
        tls:
          insecure_skip_verify: true
";
        Config::load_str(yaml).expect("otlp options should be accepted");
    }

    #[test]
    fn non_otlp_rejects_new_options() {
        let yaml = r"
pipelines:
  test:
    inputs:
      - type: file
        path: /tmp/test.log
    outputs:
      - type: stdout
        retry_attempts: 3
";
        let err = Config::load_str(yaml).unwrap_err().to_string();
        assert!(
            err.contains("unknown field") && err.contains("retry_attempts"),
            "stdout output should reject retry_attempts at parse time: {err}"
        );
    }

    #[test]
    fn elasticsearch_accepts_tls_and_request_timeout_ms() {
        let yaml = r"
pipelines:
  test:
    inputs:
      - type: file
        path: /tmp/test.log
    outputs:
      - type: elasticsearch
        endpoint: https://localhost:9200
        request_timeout_ms: 5000
        tls:
          insecure_skip_verify: true
";
        Config::load_str(yaml).expect("elasticsearch should accept tls and request_timeout_ms");
    }

    #[test]
    fn loki_accepts_tls_and_request_timeout_ms() {
        let yaml = r"
pipelines:
  test:
    inputs:
      - type: file
        path: /tmp/test.log
    outputs:
      - type: loki
        endpoint: https://localhost:3100
        request_timeout_ms: 5000
        tls:
          insecure_skip_verify: true
";
        Config::load_str(yaml).expect("loki should accept tls and request_timeout_ms");
    }

    #[test]
    fn elasticsearch_rejects_zero_request_timeout_ms() {
        let yaml = r"
pipelines:
  test:
    inputs:
      - type: file
        path: /tmp/test.log
    outputs:
      - type: elasticsearch
        endpoint: https://localhost:9200
        request_timeout_ms: 0
";
        // Now rejected at parse time via PositiveMillis.
        let _ = Config::load_str(yaml).expect_err("zero request_timeout_ms should be rejected");
    }

    #[test]
    fn otlp_rejects_zero_request_timeout_ms() {
        let yaml = r"
pipelines:
  test:
    inputs:
      - type: file
        path: /tmp/test.log
    outputs:
      - type: otlp
        endpoint: http://localhost:4317
        request_timeout_ms: 0
";
        // Now rejected at parse time via PositiveMillis.
        let _ = Config::load_str(yaml).expect_err("zero request_timeout_ms should be rejected");
    }

    #[test]
    fn otlp_rejects_zero_batch_timeout_ms() {
        let yaml = r"
pipelines:
  test:
    inputs:
      - type: file
        path: /tmp/test.log
    outputs:
      - type: otlp
        endpoint: http://localhost:4317
        batch_timeout_ms: 0
";
        // Now rejected at parse time via PositiveMillis.
        let _ = Config::load_str(yaml).expect_err("zero batch_timeout_ms should be rejected");
    }

    #[test]
    fn otlp_rejects_initial_backoff_exceeding_max() {
        let yaml = r"
pipelines:
  test:
    inputs:
      - type: file
        path: /tmp/test.log
    outputs:
      - type: otlp
        endpoint: http://localhost:4317
        retry_initial_backoff_ms: 5000
        retry_max_backoff_ms: 1000
";
        let err = Config::load_str(yaml).unwrap_err().to_string();
        assert!(
            err.contains("retry_initial_backoff_ms") && err.contains("retry_max_backoff_ms"),
            "expected backoff ordering rejection, got: {err}"
        );
    }

    #[test]
    fn arrow_ipc_accepts_batch_size() {
        let yaml = r"
pipelines:
  test:
    inputs:
      - type: file
        path: /tmp/test.log
    outputs:
      - type: arrow_ipc
        endpoint: http://localhost:9000
        batch_size: 512
";
        Config::load_str(yaml).expect("arrow_ipc should accept batch_size");
    }

    #[test]
    fn tcp_tls_accepts_cert_and_key() {
        let yaml = r#"
pipelines:
  test:
    inputs:
      - type: tcp
        listen: 127.0.0.1:5514
        tls:
          cert_file: /tmp/server.crt
          key_file: /tmp/server.key
    outputs:
      - type: "null"
"#;
        Config::load_str(yaml).expect("tcp tls cert+key should validate");
    }

    #[test]
    fn tcp_tls_rejects_partial_cert_key_pair() {
        let partial = r#"
pipelines:
  test:
    inputs:
      - type: tcp
        listen: 127.0.0.1:5514
        tls:
          cert_file: /tmp/server.crt
    outputs:
      - type: "null"
"#;
        let err = Config::load_str(partial).unwrap_err().to_string();
        assert!(
            err.contains("tls.cert_file") && err.contains("tls.key_file"),
            "expected cert/key pairing validation error, got: {err}"
        );
    }

    #[test]
    fn tcp_mtls_requires_client_ca() {
        let mtls = r#"
pipelines:
  test:
    inputs:
      - type: tcp
        listen: 127.0.0.1:5514
        tls:
          cert_file: /tmp/server.crt
          key_file: /tmp/server.key
          require_client_auth: true
    outputs:
      - type: "null"
"#;
        let err = Config::load_str(mtls).unwrap_err().to_string();
        assert!(
            err.contains("require_client_auth requires tls.client_ca_file"),
            "expected missing client CA rejection, got: {err}"
        );
    }

    #[test]
    fn tcp_mtls_accepts_client_ca_when_auth_required() {
        let mtls = r#"
pipelines:
  test:
    inputs:
      - type: tcp
        listen: 127.0.0.1:5514
        tls:
          cert_file: /tmp/server.crt
          key_file: /tmp/server.key
          client_ca_file: /tmp/ca.crt
          require_client_auth: true
    outputs:
      - type: "null"
"#;
        Config::load_str(mtls).expect("mTLS with client CA should validate");
    }

    #[test]
    fn tcp_client_ca_requires_auth_required() {
        let mtls = r#"
pipelines:
  test:
    inputs:
      - type: tcp
        listen: 127.0.0.1:5514
        tls:
          cert_file: /tmp/server.crt
          key_file: /tmp/server.key
          client_ca_file: /tmp/ca.crt
    outputs:
      - type: "null"
"#;
        let err = Config::load_str(mtls).unwrap_err().to_string();
        assert!(
            err.contains("client_ca_file requires tls.require_client_auth: true"),
            "expected client CA without mTLS rejection, got: {err}"
        );
    }

    #[test]
    fn tcp_client_ca_rejects_blank_path() {
        let mtls = r#"
pipelines:
  test:
    inputs:
      - type: tcp
        listen: 127.0.0.1:5514
        tls:
          cert_file: /tmp/server.crt
          key_file: /tmp/server.key
          client_ca_file: "   "
    outputs:
      - type: "null"
"#;
        let err = Config::load_str(mtls).unwrap_err().to_string();
        assert!(
            err.contains("client_ca_file must not be empty"),
            "expected blank client CA rejection, got: {err}"
        );
    }
}
