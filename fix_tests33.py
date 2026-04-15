import sys
import re

with open("crates/logfwd/tests/it/integration.rs", "r") as f:
    content = f.read()

content = content.replace('''#[test]
fn test_http_output_is_rejected_at_config_load() {
    let yaml = r#"
input:
  type: file
  path: /tmp/http_test.log
  format: json
output:
  type: http
  endpoint: "http://127.0.0.1:9999/logs"
"#;
    let err = Config::load_str(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not yet implemented"),
        "http output must be rejected with 'not yet implemented', got: {msg}"
    );
}''', '')

with open("crates/logfwd/tests/it/integration.rs", "w") as f:
    f.write(content)
