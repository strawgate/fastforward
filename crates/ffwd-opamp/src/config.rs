//! OpAMP configuration types.
//!
//! These are the YAML config fields that enable OpAMP on an ffwd instance.

use serde::Deserialize;

/// Configuration for the OpAMP client extension.
///
/// Added to the top-level ffwd config under `opamp:`:
///
/// ```yaml
/// opamp:
///   endpoint: "http://opamp-server:4320/v1/opamp"
///   api_key: "optional-bearer-token"
///   poll_interval_secs: 30
///   instance_uid: "auto"  # or a fixed UUID
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OpampConfig {
    /// OpAMP server endpoint URL (HTTP or WebSocket).
    pub endpoint: String,

    /// Optional API key for authentication (sent as Bearer token).
    #[serde(default)]
    pub api_key: Option<String>,

    /// Polling interval in seconds (default: 30, per OpAMP spec recommendation).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Instance UID. If "auto" or absent, a random UUID is generated and
    /// persisted in the data directory.
    #[serde(default)]
    pub instance_uid: Option<String>,

    /// Whether to accept remote configuration from the server.
    /// When false, remote config is reported but not applied.
    #[serde(default = "default_accept_remote_config")]
    pub accept_remote_config: bool,

    /// Service name reported to the OpAMP server.
    #[serde(default = "default_service_name")]
    pub service_name: String,
}

fn default_poll_interval() -> u64 {
    30
}

fn default_accept_remote_config() -> bool {
    true
}

fn default_service_name() -> String {
    "ffwd".to_string()
}

impl Default for OpampConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            api_key: None,
            poll_interval_secs: default_poll_interval(),
            instance_uid: None,
            accept_remote_config: default_accept_remote_config(),
            service_name: default_service_name(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_config() {
        let yaml = r#"
endpoint: "http://localhost:4320/v1/opamp"
"#;
        let config: OpampConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.endpoint, "http://localhost:4320/v1/opamp");
        assert_eq!(config.poll_interval_secs, 30);
        assert!(config.accept_remote_config);
        assert_eq!(config.service_name, "ffwd");
        assert!(config.api_key.is_none());
    }

    #[test]
    fn deserialize_full_config() {
        let yaml = r#"
endpoint: "http://opamp.example.com:4320/v1/opamp"
api_key: "secret-key-123"
poll_interval_secs: 60
instance_uid: "550e8400-e29b-41d4-a716-446655440000"
accept_remote_config: false
service_name: "my-ffwd-fleet"
"#;
        let config: OpampConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.endpoint, "http://opamp.example.com:4320/v1/opamp");
        assert_eq!(config.api_key.as_deref(), Some("secret-key-123"));
        assert_eq!(config.poll_interval_secs, 60);
        assert_eq!(
            config.instance_uid.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert!(!config.accept_remote_config);
        assert_eq!(config.service_name, "my-ffwd-fleet");
    }
}
