//! Errors shared by all server-side inbounds.
//!
//! Mirrors ADR-0001's rule that public signatures carry a concrete error type
//! rather than `anyhow::Error`, even though this crate is outside that workspace.

/// Result alias used throughout the server crate.
pub type Result<T> = std::result::Result<T, ServerError>;

/// Every failure mode of the server-side inbound stack.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("WebSocket handshake failed: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("unsupported VLESS version: {0}")]
    UnsupportedVersion(u8),

    #[error("truncated VLESS request header")]
    TruncatedHeader,

    #[error("invalid VLESS address type: {0}")]
    InvalidAddressType(u8),

    #[error("VLESS domain name is empty, malformed, or longer than 255 bytes")]
    InvalidDomain,

    #[error("VLESS user is not in the configured user table")]
    UnknownUser,

    /// TCP command is the only one wired so far. UDP / Mux are deliberately
    /// rejected rather than silently mis-routed (ADR-0002 class A behaviour).
    #[error("unsupported VLESS command: {0}")]
    UnsupportedCommand(u8),

    #[error("transport not implemented: {0}")]
    NotImplemented(&'static str),

    #[error("invalid user UUID: {0}")]
    InvalidUuid(#[from] uuid::Error),
}
