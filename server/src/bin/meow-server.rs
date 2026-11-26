//! Smoke binary for the server-side inbounds.
//!
//! It is built only from inside the `server/` workspace, never from the repo
//! root, so the client release binary is unaffected.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use clap::Parser;
use meow_server::config::{InboundConfig, DEFAULT_WS_PATH};
use meow_server::error::Result;
use meow_server::listener::{serve_vless_ws, DiscardHandler, SessionHandler};
use meow_server::stream::DynStream;
use meow_server::vless::{Destination, VlessRequest, COMMAND_TCP};
use meow_server::vmess;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    name = "meow-server",
    about = "meow-vgate server-side inbound smoke binary"
)]
struct Args {
    /// VLESS over WebSocket listen address.
    #[arg(long, default_value = "127.0.0.1:19001")]
    vless_ws: SocketAddr,

    /// WebSocket upgrade path required from clients.
    #[arg(long, default_value = DEFAULT_WS_PATH)]
    ws_path: String,

    /// VLESS user UUID; repeat the flag for multiple users.
    #[arg(long, value_name = "UUID")]
    uuid: Vec<String>,

    /// Relay accepted sessions to their destination over plain TCP instead
    /// of discarding them (smoke-only end-to-end test mode).
    #[arg(long, default_value_t = false)]
    relay: bool,

    /// Accepted only to fail loudly: the smoke binary does not serve
    /// VMess/mKCP (no tunnel to relay into). Use the meow-app
    /// `server-inbound` build for the real thing.
    #[arg(long, value_name = "ADDR")]
    vmess_mkcp: Option<SocketAddr>,
}

/// Smoke-only relay handler: opens the VLESS destination with a plain TCP
/// socket and copies bytes both ways. This lets a real mihomo client verify
/// "connect + data flows through" without needing the full client tunnel.
struct RelayHandler;

#[async_trait]
impl SessionHandler for RelayHandler {
    async fn handle(&self, request: VlessRequest, mut stream: DynStream) -> Result<()> {
        if request.command != COMMAND_TCP {
            return Err(meow_server::error::ServerError::UnsupportedCommand(
                request.command,
            ));
        }
        let host = match &request.destination {
            Destination::Ip(ip) => ip.to_string(),
            Destination::Domain(domain) => domain.clone(),
        };
        let target = format!("{host}:{}", request.port);
        let mut upstream = TcpStream::connect(&target)
            .await
            .map_err(|e| std::io::Error::new(e.kind(), format!("connect {target}: {e}")))?;
        let (sent, received) = copy_bidirectional(&mut stream, &mut upstream).await?;
        eprintln!("relay {target}: {sent} bytes up, {received} bytes down");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.uuid.is_empty() {
        return Err("at least one --uuid is required".into());
    }
    if args.vmess_mkcp.is_some() {
        return Err(vmess::not_implemented().into());
    }

    let users = args
        .uuid
        .iter()
        .map(|value| Uuid::parse_str(value).map(|uuid| *uuid.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let listener = TcpListener::bind(args.vless_ws).await?;
    eprintln!(
        "meow-server: VLESS/WS listening on {} (path {:?}), {} user(s)",
        args.vless_ws,
        args.ws_path,
        users.len()
    );

    let config = InboundConfig {
        listen: args.vless_ws,
        ws_path: args.ws_path,
    };
    if args.relay {
        eprintln!("meow-server: relay mode enabled (plain TCP upstream)");
        serve_vless_ws(listener, config, users, Arc::new(RelayHandler)).await?;
    } else {
        serve_vless_ws(listener, config, users, Arc::new(DiscardHandler)).await?;
    }
    Ok(())
}
