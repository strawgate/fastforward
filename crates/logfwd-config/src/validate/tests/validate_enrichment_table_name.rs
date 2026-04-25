#[cfg(test)]
mod validate_enrichment_table_name_tests {
    use crate::types::Config;

    #[test]
    fn enrichment_static_empty_table_name_rejected() {
        let yaml = r"
pipelines:
  app:
    inputs:
      - type: file
        path: /tmp/x.log
    outputs:
      - type: stdout
    enrichment:
      - type: static
        table_name: ''
        labels:
          key: val
";
        let err = Config::load_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("table_name must not be empty"),
            "expected 'table_name must not be empty' in error: {msg}"
        );
    }

    #[test]
    fn enrichment_csv_empty_table_name_rejected() {
        let yaml = r"
pipelines:
  app:
    inputs:
      - type: file
        path: /tmp/x.log
    outputs:
      - type: stdout
    enrichment:
      - type: csv
        table_name: ''
        path: relative/path/assets.csv
";
        let err = Config::load_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("table_name must not be empty"),
            "expected 'table_name must not be empty' in error: {msg}"
        );
    }

    #[test]
    fn enrichment_jsonl_empty_table_name_rejected() {
        let yaml = r"
pipelines:
  app:
    inputs:
      - type: file
        path: /tmp/x.log
    outputs:
      - type: stdout
    enrichment:
      - type: jsonl
        table_name: ''
        path: relative/path/ips.jsonl
";
        let err = Config::load_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("table_name must not be empty"),
            "expected 'table_name must not be empty' in error: {msg}"
        );
    }
}
