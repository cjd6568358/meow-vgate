//! VMess inbound status for the standalone smoke binary.
//!
//! The real VMess-over-mKCP server inbound — AEAD request-header open,
//! response-header seal, chunk-stream body relay into the shared tunnel —
//! lives in the meow-app fork delta (`crates/meow-app/src/vmess_inbound.rs`,
//! feature `server-inbound`) and reuses meow-proxy's golden-vector-locked
//! VMess primitives. The mKCP transport it runs on is this crate's
//! [`crate::mkcp`] module (porting mihomo v1.19.34 `transport/mkcp`).
//!
//! This module only keeps the loud failure the standalone smoke binary
//! reports when `--vmess-mkcp` is requested: that binary serves VLESS/WS
//! relay smoke tests and has no tunnel to route VMess into.

use crate::error::ServerError;

/// Explicit failure returned by the smoke binary for `--vmess-mkcp`.
pub fn not_implemented() -> ServerError {
    ServerError::NotImplemented(
        "vmess over mkcp (use the meow-app `server-inbound` build, not the smoke binary)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_bin_failure_is_explicit() {
        assert!(matches!(not_implemented(), ServerError::NotImplemented(_)));
    }
}
