#[cfg(test)]
mod validate_http_response_tests {
    use crate::types::Config;

    #[test]
    fn http_response_body_with_204_is_rejected() {
        let yaml = r#"
pipelines:
  test:
    inputs:
      - type: http
        listen: 127.0.0.1:8081
        format: json
        http:
          path: /ingest
          response_code: 204
          response_body: '{"ok":true}'
    outputs:
      - type: "null"
"#;
        let err = Config::load_str(yaml).expect_err("204 + response_body must fail validation");
        assert!(
            err.to_string()
                .contains("http.response_body is not allowed when http.response_code is 204"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn http_max_drained_bytes_per_poll_zero_is_rejected() {
        let yaml = r#"
pipelines:
  test:
    inputs:
      - type: http
        listen: 127.0.0.1:8081
        format: json
        http:
          max_drained_bytes_per_poll: 0
    outputs:
      - type: "null"
"#;
        let err = Config::load_str(yaml).expect_err("zero drain cap must fail validation");
        assert!(
            err.to_string()
                .contains("http.max_drained_bytes_per_poll must be at least 1"),
            "unexpected error: {err}"
        );
    }
}
