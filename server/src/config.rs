//! Minimal inbound configuration for the server crate.
//!
//! This is deliberately not `meow-config::Config`: the client config schema is
//! not a server schema, and reusing it would drag the whole config crate (and
//! its dependency tree) into the server workspace. YAML parsing is a later
//! card; today the binary assembles these structs from CLI arguments.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Deployment port for VLESS over WebSocket.
pub const DEFAULT_VLESS_WS_PORT: u16 = 19001;
/// Deployment port for VMess over mKCP (placeholder, see [`crate::vmess`]).
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

impl InboundConfig {
    pub fn vless_ws() -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_VLESS_WS_PORT),
            ws_path: DEFAULT_WS_PATH.to_owned(),
        }
    }

    pub fn vmess_mkcp() -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_VMESS_MKCP_PORT),
            ws_path: DEFAULT_WS_PATH.to_owned(),
        }
    }
}

/// Whole-server configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerConfig {
    pub vless_ws: Option<InboundConfig>,
    pub vmess_mkcp: Option<InboundConfig>,
    /// VLESS user IDs in raw 16-byte form (mihomo/sing-vless UUID bytes).
    pub users: Vec<[u8; 16]>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_ports_are_pinned() {
        assert_eq!(DEFAULT_VLESS_WS_PORT, 19001);
        assert_eq!(DEFAULT_VMESS_MKCP_PORT, 19002);
    }

    #[test]
    fn default_vless_ws_config_matches_deployment_contract() {
        let config = InboundConfig::vless_ws();
        assert_eq!(config.listen.port(), 19001);
        assert_eq!(config.ws_path, "/ws");
    }

    #[test]
    fn default_vmess_mkcp_config_uses_19002() {
        assert_eq!(InboundConfig::vmess_mkcp().listen.port(), 19002);
    }

    #[test]
    fn server_config_starts_empty() {
        let config = ServerConfig::default();
        assert!(config.vless_ws.is_none());
        assert!(config.vmess_mkcp.is_none());
        assert!(config.users.is_empty());
    }
}
