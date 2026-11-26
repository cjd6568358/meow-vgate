//! Minimal inbound configuration for the server crate.
//!
//! This is deliberately not `meow-config::Config`: the client config schema
//! is not a server schema, and reusing it would drag the whole config crate
//! (and its dependency tree) into the server workspace. The standalone
//! binary assembles [`InboundConfig`] from CLI arguments; the meow-app
//! `server-inbound` build constructs it from `VlessListenerConfig`.

use std::net::SocketAddr;

/// Deployment port for VLESS over WebSocket. Contract anchor pinned by the
/// test below and referenced by server/docs/ARCHITECTURE.md §6.
pub const DEFAULT_VLESS_WS_PORT: u16 = 19001;
/// Deployment port for VMess over mKCP. Contract anchor, same as above.
pub const DEFAULT_VMESS_MKCP_PORT: u16 = 19002;
/// Default WebSocket upgrade path used by the deployment contract.
pub const DEFAULT_WS_PATH: &str = "/ws";

/// One TCP-facing inbound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundConfig {
    pub listen: SocketAddr,
    /// Only meaningful for WebSocket inbounds; ignored elsewhere.
    pub ws_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_contract_is_pinned() {
        assert_eq!(DEFAULT_VLESS_WS_PORT, 19001);
        assert_eq!(DEFAULT_VMESS_MKCP_PORT, 19002);
        assert_eq!(DEFAULT_WS_PATH, "/ws");
    }
}
