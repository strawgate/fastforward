//! Agent identity management.
//!
//! OpAMP requires each agent instance to have a unique, stable identifier.
//! This module handles generating, persisting, and loading instance UIDs.

use std::path::{Path, PathBuf};

use uuid::Uuid;

/// Represents the identity of this agent instance for OpAMP communication.
#[derive(Debug, Clone)]
pub struct AgentIdentity {
    /// Unique instance identifier (16-byte UUID).
    pub instance_uid: [u8; 16],

    /// Human-readable service name.
    pub service_name: String,

    /// Agent version string.
    pub version: String,
}

impl AgentIdentity {
    /// Create a new agent identity.
    ///
    /// If `configured_uid` is `Some` and parseable as a UUID, uses that.
    /// If `configured_uid` is `None` or `"auto"`, loads from `data_dir/opamp_instance_uid`
    /// or generates a new one and persists it.
    pub fn resolve(
        configured_uid: Option<&str>,
        data_dir: Option<&Path>,
        service_name: &str,
        version: &str,
    ) -> Self {
        let instance_uid = match configured_uid {
            Some(uid) if uid != "auto" && !uid.is_empty() => {
                // Try to parse as UUID.
                match Uuid::parse_str(uid) {
                    Ok(u) => *u.as_bytes(),
                    Err(_) => {
                        tracing::warn!(
                            uid = uid,
                            "opamp: configured instance_uid is not a valid UUID, generating new"
                        );
                        Self::load_or_generate(data_dir)
                    }
                }
            }
            _ => Self::load_or_generate(data_dir),
        };

        Self {
            instance_uid,
            service_name: service_name.to_string(),
            version: version.to_string(),
        }
    }

    /// Returns the instance UID as a hex string (for logging).
    pub fn uid_hex(&self) -> String {
        hex::encode(self.instance_uid)
    }

    fn uid_file_path(data_dir: Option<&Path>) -> Option<PathBuf> {
        data_dir.map(|d| d.join("opamp_instance_uid"))
    }

    fn load_or_generate(data_dir: Option<&Path>) -> [u8; 16] {
        // Try to load from file.
        if let Some(contents) =
            Self::uid_file_path(data_dir).and_then(|path| std::fs::read_to_string(path).ok())
            && let Ok(uuid) = Uuid::parse_str(contents.trim())
        {
            tracing::debug!(uid = %uuid, "opamp: loaded instance UID from file");
            return *uuid.as_bytes();
        }

        // Generate a new UUID v4.
        let uuid = Uuid::new_v4();
        tracing::info!(uid = %uuid, "opamp: generated new instance UID");

        // Persist to file if possible.
        if let Some(path) = Self::uid_file_path(data_dir) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&path, uuid.to_string()) {
                tracing::warn!(error = %e, "opamp: failed to persist instance UID");
            }
        }

        *uuid.as_bytes()
    }
}

// We don't use the `hex` crate — implement inline.
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        use std::fmt::Write;
        let bytes = bytes.as_ref();
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolve_with_explicit_uuid() {
        let identity = AgentIdentity::resolve(
            Some("550e8400-e29b-41d4-a716-446655440000"),
            None,
            "ffwd",
            "0.1.0",
        );
        let expected = *Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000")
            .expect("valid test UUID")
            .as_bytes();
        assert_eq!(identity.instance_uid, expected);
    }

    #[test]
    fn resolve_auto_generates_and_persists() {
        let dir = TempDir::new().expect("create temp dir");
        let identity1 = AgentIdentity::resolve(Some("auto"), Some(dir.path()), "ffwd", "0.1.0");

        // Second call should load the same UID.
        let identity2 = AgentIdentity::resolve(None, Some(dir.path()), "ffwd", "0.1.0");
        assert_eq!(identity1.instance_uid, identity2.instance_uid);

        // File should exist.
        let uid_path = dir.path().join("opamp_instance_uid");
        assert!(uid_path.exists());
    }

    #[test]
    fn resolve_without_data_dir_generates_random() {
        let id1 = AgentIdentity::resolve(None, None, "ffwd", "0.1.0");
        let id2 = AgentIdentity::resolve(None, None, "ffwd", "0.1.0");
        // Each call generates a new random UID since there's no persistence.
        assert_ne!(id1.instance_uid, id2.instance_uid);
    }

    #[test]
    fn uid_hex_format() {
        let identity = AgentIdentity::resolve(
            Some("550e8400-e29b-41d4-a716-446655440000"),
            None,
            "ffwd",
            "0.1.0",
        );
        assert_eq!(identity.uid_hex(), "550e8400e29b41d4a716446655440000");
    }
}
