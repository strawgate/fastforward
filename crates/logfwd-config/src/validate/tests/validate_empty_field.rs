#[cfg(test)]
mod validate_empty_field_tests {
    use crate::types::Config;

    #[test]
    fn file_input_empty_path_rejected() {
        let yaml = r#"
pipelines:
  test:
    inputs:
      - type: file
        path: ""
    outputs:
      - type: stdout
"#;
        let err = Config::load_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("path"),
            "expected path rejection: {err}"
        );
        assert!(
            err.to_string().contains("must not be empty"),
            "expected 'must not be empty' message: {err}"
        );
    }

    #[test]
    fn file_input_whitespace_path_rejected() {
        let yaml = "pipelines:\n  test:\n    inputs:\n      - type: file\n        path: \"   \"\n    outputs:\n      - type: stdout\n";
        let err = Config::load_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("path") && err.to_string().contains("must not be empty"),
            "whitespace-only path must be rejected: {err}"
        );
    }

    #[test]
    fn elasticsearch_empty_index_rejected() {
        let yaml = r#"
pipelines:
  test:
    inputs:
      - type: file
        path: /tmp/test.log
    outputs:
      - type: elasticsearch
        endpoint: http://localhost:9200
        index: ""
"#;
        let err = Config::load_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("index"),
            "expected index rejection: {err}"
        );
        assert!(
            err.to_string().contains("must not be empty"),
            "expected 'must not be empty' message: {err}"
        );
    }

    #[test]
    fn elasticsearch_whitespace_index_rejected() {
        let yaml = "pipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n    outputs:\n      - type: elasticsearch\n        endpoint: http://localhost:9200\n        index: \"   \"\n";
        let err = Config::load_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("index") && err.to_string().contains("must not be empty"),
            "whitespace-only index must be rejected: {err}"
        );
    }

    #[test]
    fn elasticsearch_index_prefix_rejected() {
        let yaml = r#"
pipelines:
  test:
    inputs:
      - type: file
        path: /tmp/test.log
    outputs:
      - type: elasticsearch
        endpoint: http://localhost:9200
        index: "_bad-index"
"#;
        let err = Config::load_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("has illegal prefix '_'"),
            "expected prefix rejection: {err}"
        );
    }
}
