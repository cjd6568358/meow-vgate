//! FORK DELTA (meow-vgate): single-process VLESS server inbound.
//!
//! Implements the `SessionHandler` bridge inside the app crate, which already
//! depends on every meow crate. The standalone `meow-server` crate stays free
//! of upstream dependencies (so it can be checked on its own); only this
//! feature-gated module links it into the shared `Tunnel`.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result as AnyhowResult;
use async_trait::async_trait;
use meow_common::{ConnType, Metadata, Network};
use meow_config::{VlessListenerConfig, VmessListenerConfig};
use meow_server::error::ServerError;
use meow_server::listener::{serve_vless_ws, SessionHandler};
use meow_server::stream::DynStream;
use meow_server::vless::{Destination, VlessRequest};
use meow_tunnel::Tunnel;
use smol_str::SmolStr;

/// Session handler that feeds an authenticated VLESS stream into the shared
/// client tunnel (rules, DNS, outbound adapters, and connection stats).
pub struct RouteSink {
    tunnel: Tunnel,
    in_name: SmolStr,
    in_port: u16,
}

impl RouteSink {
    pub fn new(tunnel: Tunnel, in_name: impl Into<SmolStr>, in_port: u16) -> Self {
        Self {
            tunnel,
            in_name: in_name.into(),
            in_port,
        }
    }

    fn metadata(&self, request: &VlessRequest, peer: Option<SocketAddr>) -> Metadata {
        build_metadata(request, peer, &self.in_name, self.in_port)
    }
}

fn build_metadata(
    request: &VlessRequest,
    peer: Option<SocketAddr>,
    in_name: &SmolStr,
    in_port: u16,
) -> Metadata {
    let (host, dst_ip) = match &request.destination {
        Destination::Domain(name) => (SmolStr::from(name.as_str()), None),
        Destination::Ip(ip) => (SmolStr::from(ip.to_string()), Some(*ip)),
    };
    Metadata {
        network: Network::Tcp,
        conn_type: ConnType::Vless,
        src_ip: peer.map(|address| address.ip()),
        src_port: peer.map_or(0, |address| address.port()),
        dst_ip,
        dst_port: request.port,
        host,
        in_name: in_name.clone(),
        in_port,
        ..Default::default()
    }
}

#[async_trait]
impl SessionHandler for RouteSink {
    async fn handle(
        &self,
        request: VlessRequest,
        mut stream: DynStream,
    ) -> meow_server::Result<()> {
        // Peer address is not retained through the handshake; source metadata
        // is best-effort (None / port 0), like other listeners before sniff.
        let metadata = self.metadata(&request, None);
        meow_tunnel::route_inbound_tcp(self.tunnel.inner(), &mut stream, metadata, &[]).await;
        Ok(())
    }
}

/// Resolve the configured UUID strings into the raw 16-byte form the VLESS
/// handshake compares against.
pub fn parse_users(users: &[meow_config::InboundUserConfig]) -> AnyhowResult<Vec<[u8; 16]>> {
    users
        .iter()
        .map(|user| {
            let uuid = uuid::Uuid::parse_str(&user.uuid)
                .map_err(|e| anyhow::anyhow!("invalid VLESS user UUID '{}': {e}", user.uuid))?;
            Ok(*uuid.as_bytes())
        })
        .collect()
}

/// Serve the VLESS listener into the shared tunnel.
pub async fn serve_vless(
    socket: tokio::net::TcpListener,
    config: &VlessListenerConfig,
    users: Vec<[u8; 16]>,
    tunnel: Tunnel,
) -> meow_server::Result<()> {
    let in_port = socket.local_addr().map_or(0, |address| address.port());
    let handler = Arc::new(RouteSink::new(tunnel, SmolStr::from("vless-ws"), in_port));
    let ws_path = config.ws_path.clone().unwrap_or_else(|| "/ws".to_owned());
    serve_vless_ws(
        socket,
        meow_server::config::InboundConfig {
            listen: "127.0.0.1:0".parse().expect("fixed placeholder address"),
            ws_path,
        },
        users,
        handler,
    )
    .await
}

/// VMess/mKCP is intentionally explicit: the config parses, but starting the
/// listener fails loudly until the mKCP stack is implemented.
pub fn vmess_not_implemented(_config: &VmessListenerConfig) -> ServerError {
    ServerError::NotImplemented("vmess over mkcp (server inbound)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use meow_server::vless::{self, ADDRESS_IPV4, COMMAND_TCP, VERSION};

    fn sample_request() -> VlessRequest {
        let mut bytes = vec![VERSION];
        bytes.extend_from_slice(&[1u8; 16]);
        bytes.push(0);
        bytes.push(COMMAND_TCP);
        bytes.extend_from_slice(&443u16.to_be_bytes());
        bytes.push(ADDRESS_IPV4);
        bytes.extend_from_slice(&[1, 1, 1, 1]);
        let (request, _) = vless::VlessRequest::decode(&bytes).expect("valid request");
        request
    }

    #[test]
    fn metadata_maps_vless_destination() {
        let metadata = build_metadata(
            &sample_request(),
            Some("10.0.0.2:1234".parse().expect("valid addr")),
            &SmolStr::from("vless-ws"),
            19001,
        );
        assert_eq!(metadata.conn_type, ConnType::Vless);
        assert_eq!(metadata.network, Network::Tcp);
        assert_eq!(metadata.dst_port, 443);
        assert_eq!(metadata.host, "1.1.1.1");
        assert_eq!(metadata.in_port, 19001);
        assert_eq!(metadata.src_port, 1234);
    }
}
