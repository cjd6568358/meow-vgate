//! Accept loops and the session-handler boundary for server inbounds.
//!
//! The listener deliberately stops at `SessionHandler`: tunnelling, rule
//! matching and outbound dialling belong to the client kernel and are not
//! dragged into this workspace. A handler can echo for tests, discard for a
//! smoke binary, or eventually bridge to a routing table in a later card.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use crate::config::InboundConfig;
use crate::error::{Result, ServerError};
use crate::stream::DynStream;
use crate::transport::{ServerTransport, WsAccept};
use crate::vless::{self, VlessRequest, COMMAND_TCP, RESPONSE_HEADER};

/// Per-connection entry point after the VLESS handshake has been authenticated.
#[async_trait]
pub trait SessionHandler: Send + Sync {
    async fn handle(&self, request: VlessRequest, stream: DynStream) -> Result<()>;
}

/// Default handler used by the smoke binary: log the destination, drop the
/// stream. It exists so startup is testable without pretending a tunnel exists.
pub struct DiscardHandler;

#[async_trait]
impl SessionHandler for DiscardHandler {
    async fn handle(&self, request: VlessRequest, _stream: DynStream) -> Result<()> {
        tracing::info!(
            port = request.port,
            destination = ?request.destination,
            "VLESS session accepted (no relay wired yet)"
        );
        Ok(())
    }
}

/// Serve VLESS over WebSocket until the listener is closed.
///
/// Each accepted TCP connection is upgraded to WebSocket, then parsed as one
/// VLESS request. Unsupported commands and unknown users are rejected before
/// the response header is written, so a client cannot mistake a rejection for
/// an established session.
pub async fn serve_vless_ws(
    listener: TcpListener,
    config: InboundConfig,
    users: Vec<[u8; 16]>,
    handler: Arc<dyn SessionHandler>,
) -> Result<()> {
    let users = Arc::new(users);
    loop {
        let (tcp, peer) = listener.accept().await?;
        let ws_path = config.ws_path.clone();
        let users = Arc::clone(&users);
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            if let Err(error) = handle_connection(tcp, ws_path, users, handler).await {
                tracing::warn!(%peer, %error, "VLESS/WS session failed");
            }
        });
    }
}

async fn handle_connection(
    tcp: TcpStream,
    ws_path: String,
    users: Arc<Vec<[u8; 16]>>,
    handler: Arc<dyn SessionHandler>,
) -> Result<()> {
    let mut stream = WsAccept::new(ws_path).accept(Box::new(tcp)).await?;
    let request = vless::read_request(&mut stream).await?;

    if !request.authenticate(users.as_slice()) {
        return Err(ServerError::UnknownUser);
    }
    if request.command != COMMAND_TCP {
        return Err(ServerError::UnsupportedCommand(request.command));
    }

    stream.write_all(&RESPONSE_HEADER).await?;
    handler.handle(request, stream).await
}
