#[cfg(test)]
mod feedback_loop_tests {
    use crate::types::Config;
    use crate::validate::is_glob_match_possible;

    #[test]
    fn file_output_same_as_input_rejected() {
        let yaml = r"
pipelines:
  looping:
    inputs:
      - type: file
        path: /tmp/logfwd-feedback-test.log
    outputs:
      - type: file
        path: /tmp/logfwd-feedback-test.log
        format: json
";
        let err = Config::load_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("feedback loop") || msg.contains("same as file input"),
            "expected feedback-loop rejection, got: {msg}"
        );
    }

    #[test]
    fn file_output_different_from_input_allowed() {
        let yaml = r"
pipelines:
  ok:
    inputs:
      - type: file
        path: /tmp/logfwd-input.log
    outputs:
      - type: file
        path: /tmp/logfwd-output.log
        format: json
";
        Config::load_str(yaml).expect("different input/output paths should be allowed");
    }

    #[test]
    fn file_output_same_as_input_rejected_after_lexical_normalization() {
        let yaml = r"
pipelines:
  looping:
    inputs:
      - type: file
        path: ./tmp/logs/../app.log
    outputs:
      - type: file
        path: tmp/./app.log
        format: json
";
        let err = Config::load_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("feedback loop") || msg.contains("same as file input"),
            "expected normalized-path feedback-loop rejection, got: {msg}"
        );
    }

    #[test]
    fn glob_could_match_literal_filename_requires_exact_name() {
        assert!(is_glob_match_possible(
            "/var/log/access.log",
            "/var/log/access.log"
        ));
        assert!(!is_glob_match_possible(
            "/var/log/access.log",
            "/var/log/other.log"
        ));
    }

    #[test]
    fn glob_could_match_wildcard_suffix_pattern() {
        assert!(is_glob_match_possible(
            "/var/log/*.log",
            "/var/log/access.log"
        ));
        assert!(!is_glob_match_possible(
            "/var/log/*.log",
            "/var/log/access.txt"
        ));
    }

    #[test]
    fn glob_could_match_rejects_different_directory() {
        assert!(!is_glob_match_possible("/var/log/*.log", "/tmp/access.log"));
    }

    #[test]
    fn glob_could_match_nested_wildcards_are_conservative() {
        assert!(is_glob_match_possible(
            "/var/log/*test*.log",
            "/var/log/mytest_file.log"
        ));
    }

    #[test]
    fn glob_could_match_recursive_double_star_matches_nested_directories() {
        assert!(is_glob_match_possible(
            "/var/log/**/access.log",
            "/var/log/subdir/access.log"
        ));
        assert!(!is_glob_match_possible(
            "/var/log/**/access.log",
            "/var/log/subdir/error.log"
        ));
    }

    #[test]
    fn glob_could_match_recursive_double_star_directory_only_pattern() {
        assert!(is_glob_match_possible(
            "/var/log/**",
            "/var/log/subdir/app.log"
        ));
        assert!(is_glob_match_possible("/var/log/**", "/var/log/app.log"));
        assert!(!is_glob_match_possible("/var/log/**", "/srv/log/app.log"));
    }

    #[test]
    fn glob_could_match_directory_wildcards_are_conservative() {
        assert!(is_glob_match_possible("/var/*/app.log", "/var/log/app.log"));
        assert!(is_glob_match_possible(
            "/var/log[12]/*.log",
            "/var/log1/a.log"
        ));
        assert!(!is_glob_match_possible(
            "/var/*/app.log",
            "/srv/log/app.log"
        ));
    }
}
