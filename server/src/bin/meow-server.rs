//! Smoke binary for the server-side inbounds.
//!
//! It is built only from inside the `server/` workspace, never from the repo
//! root, so the client release binary is unaffected.

use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use meow_server::config::{InboundConfig, DEFAULT_WS_PATH};
use meow_server::listener::{serve_vless_ws, DiscardHandler};
use meow_server::vmess;
use tokio::net::TcpListener;
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

    /// Accepted only to fail loudly: mKCP inbound is not implemented yet.
    #[arg(long, value_name = "ADDR")]
    vmess_mkcp: Option<SocketAddr>,
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

    serve_vless_ws(
        listener,
        InboundConfig {
            listen: args.vless_ws,
            ws_path: args.ws_path,
        },
        users,
        Arc::new(DiscardHandler),
    )
    .await?;
    Ok(())
}
