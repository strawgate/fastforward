//! OpAMP error types.

/// Errors that can occur during OpAMP client operation.
#[derive(Debug, thiserror::Error)]
pub enum OpampError {
    /// Failed to connect to the OpAMP server.
    #[error("opamp connection failed: {0}")]
    Connection(String),

    /// Failed to serialize or deserialize OpAMP messages.
    #[error("opamp serialization error: {0}")]
    Serialization(String),

    /// The server rejected the agent's connection.
    #[error("opamp server rejected connection: {0}")]
    Rejected(String),

    /// Invalid configuration received from the server.
    #[error("opamp invalid remote config: {0}")]
    InvalidConfig(String),

    /// The reload channel is closed (runtime shutting down).
    #[error("reload channel closed")]
    ReloadChannelClosed,
}
