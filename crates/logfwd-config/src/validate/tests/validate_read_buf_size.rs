#[cfg(test)]
mod validate_read_buf_size_tests {
    use crate::types::Config;

    #[test]
    fn read_buf_size_upper_bound_rejected() {
        let yaml = "pipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n        read_buf_size: 5000000\n    outputs:\n      - type: stdout\n";
        let err = Config::load_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("read_buf_size") && msg.contains("4194304"),
            "expected read_buf_size upper bound rejection: {msg}"
        );
    }

    #[test]
    fn read_buf_size_at_max_accepted() {
        let yaml = "pipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n        read_buf_size: 4194304\n    outputs:\n      - type: stdout\n";
        Config::load_str(yaml).expect("read_buf_size at exactly 4 MiB should be accepted");
    }

    #[test]
    fn read_buf_size_just_over_max_rejected() {
        let yaml = "pipelines:\n  test:\n    inputs:\n      - type: file\n        path: /tmp/test.log\n        read_buf_size: 4194305\n    outputs:\n      - type: stdout\n";
        let err = Config::load_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("read_buf_size") && msg.contains("4194304"),
            "expected read_buf_size upper bound rejection: {msg}"
        );
    }
}
