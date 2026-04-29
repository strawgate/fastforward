//! Tests for the config reload lifecycle.
//!
//! These tests exercise the reload path including:
//! - ConfigDiff detection of changes
//! - Reload with generator pipelines (non-file, no I/O)
//! - Invalid config rejection

use std::time::Duration;

use ffwd_config::Config;
use ffwd_runtime::pipeline::Pipeline;
use opentelemetry::metrics::MeterProvider;
use tokio_util::sync::CancellationToken;

fn test_meter() -> opentelemetry::metrics::Meter {
    opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .build()
        .meter("test")
}

/// A pipeline can be built, run briefly, and then rebuilt from a new config.
#[tokio::test]
async fn pipeline_rebuild_after_drain() {
    let yaml_v1 = r#"
pipelines:
  test:
    inputs:
      - type: generator
        generator:
          total_events: 5
          batch_size: 5
    outputs:
      - type: "null"
"#;
    let yaml_v2 = r#"
pipelines:
  test:
    inputs:
      - type: generator
        generator:
          total_events: 10
          batch_size: 5
    outputs:
      - type: "null"
"#;

    let meter = test_meter();
    let config_v1 = Config::load_str(yaml_v1).expect("parse v1");
    let config_v2 = Config::load_str(yaml_v2).expect("parse v2");

    // Build and run v1.
    let mut pipeline_v1 =
        Pipeline::from_config_with_data_dir("test", &config_v1.pipelines["test"], &meter, None, None)
            .expect("build v1");
    let shutdown = CancellationToken::new();
    let result = tokio::time::timeout(Duration::from_secs(10), pipeline_v1.run_async(&shutdown)).await;
    assert!(result.is_ok(), "v1 should finish");

    // Build and run v2 (simulating a reload).
    let mut pipeline_v2 =
        Pipeline::from_config_with_data_dir("test", &config_v2.pipelines["test"], &meter, None, None)
            .expect("build v2");
    let shutdown2 = CancellationToken::new();
    let result = tokio::time::timeout(Duration::from_secs(10), pipeline_v2.run_async(&shutdown2)).await;
    assert!(result.is_ok(), "v2 should finish");
}

/// ConfigDiff correctly identifies when pipelines change.
#[test]
fn config_diff_pipeline_change() {
    let yaml_v1 = r#"
pipelines:
  alpha:
    inputs:
      - type: generator
        generator:
          total_events: 5
    outputs:
      - type: "null"
  beta:
    inputs:
      - type: generator
        generator:
          total_events: 5
    outputs:
      - type: "null"
"#;
    let yaml_v2 = r#"
pipelines:
  alpha:
    inputs:
      - type: generator
        generator:
          total_events: 5
    outputs:
      - type: "null"
  beta:
    inputs:
      - type: generator
        generator:
          total_events: 100
    outputs:
      - type: "null"
  gamma:
    inputs:
      - type: generator
        generator:
          total_events: 5
    outputs:
      - type: "null"
"#;

    let config_v1 = Config::load_str(yaml_v1).expect("parse v1");
    let config_v2 = Config::load_str(yaml_v2).expect("parse v2");
    let diff = ffwd_config::ConfigDiff::between(&config_v1, &config_v2);

    assert_eq!(diff.unchanged, vec!["alpha"]);
    assert_eq!(diff.changed, vec!["beta"]);
    assert_eq!(diff.added, vec!["gamma"]);
    assert!(diff.removed.is_empty());
    assert!(!diff.is_empty());
    assert!(diff.is_reloadable());
}

/// ConfigDiff detects removal of pipelines.
#[test]
fn config_diff_pipeline_removal() {
    let yaml_v1 = r#"
pipelines:
  alpha:
    inputs:
      - type: generator
        generator:
          total_events: 5
    outputs:
      - type: "null"
  beta:
    inputs:
      - type: generator
        generator:
          total_events: 5
    outputs:
      - type: "null"
"#;
    let yaml_v2 = r#"
pipelines:
  alpha:
    inputs:
      - type: generator
        generator:
          total_events: 5
    outputs:
      - type: "null"
"#;

    let config_v1 = Config::load_str(yaml_v1).expect("parse v1");
    let config_v2 = Config::load_str(yaml_v2).expect("parse v2");
    let diff = ffwd_config::ConfigDiff::between(&config_v1, &config_v2);

    assert_eq!(diff.unchanged, vec!["alpha"]);
    assert_eq!(diff.removed, vec!["beta"]);
    assert!(diff.added.is_empty());
    assert!(diff.changed.is_empty());
}

/// Invalid config fails to parse (simulating a rejected reload).
#[test]
fn invalid_config_fails_gracefully() {
    let invalid_yaml = "this is not: valid: [[[[yaml";
    let result = Config::load_str(invalid_yaml);
    assert!(result.is_err(), "invalid YAML should fail to parse");
}

/// Config missing pipelines section fails validation.
#[test]
fn missing_pipelines_fails_validation() {
    let yaml = r#"
server:
  diagnostics: "127.0.0.1:8686"
"#;
    let result = Config::load_str(yaml);
    assert!(result.is_err(), "config without pipelines should fail");
}

/// Empty pipelines object fails validation (requires at least one pipeline).
#[test]
fn empty_pipelines_fails_validation() {
    let yaml = r#"
pipelines: {}
"#;
    let result = Config::load_str(yaml);
    assert!(result.is_err(), "empty pipelines should fail validation");
}

/// OpAMP config section is ignored when not configured.
#[test]
fn opamp_section_is_optional() {
    let yaml_without = r#"
pipelines:
  test:
    inputs:
      - type: generator
        generator:
          total_events: 5
    outputs:
      - type: "null"
"#;
    let yaml_with = r#"
pipelines:
  test:
    inputs:
      - type: generator
        generator:
          total_events: 5
    outputs:
      - type: "null"
opamp:
  endpoint: http://localhost:4320/v1/opamp
"#;

    let config_without = Config::load_str(yaml_without).expect("parse without opamp");
    let config_with = Config::load_str(yaml_with).expect("parse with opamp");

    assert!(config_without.opamp.is_none());
    assert!(config_with.opamp.is_some());

    // They should not be equal (opamp differs).
    assert_ne!(config_without, config_with);
}
