//! VMess over mKCP inbound — typed placeholder for port 19002.
//!
//! Nothing here is an implementation. The seam exists so that configuration
//! and startup can fail loudly instead of silently accepting connections it
//! cannot serve. Reference material (read-only) is
//! `server/zig-docs/source/listener/vmess/mkcp_server.zig` and
//! `server/zig-docs/go-baseline/transport/mkcp/`.
//!
//! kcp-go v5.6.72 compatible ARQ + auth + crypt + smux is the largest single
//! remaining work item; meow-transport only ships a kcptun *client* plugin, so
//! there is no Rust server-side stack to reuse.

use async_trait::async_trait;

use crate::error::{Result, ServerError};
use crate::stream::DynStream;
use crate::transport::ServerTransport;

/// Placeholder accept layer for VMess over mKCP.
#[derive(Debug, Clone, Copy)]
pub struct MkcpAccept;

#[async_trait]
impl ServerTransport for MkcpAccept {
    async fn accept(&self, _inner: DynStream) -> Result<DynStream> {
        Err(not_implemented())
    }
}

/// Shared error used by startup and by the placeholder transport.
pub fn not_implemented() -> ServerError {
    ServerError::NotImplemented("vmess over mkcp (port 19002)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_is_explicit() {
        assert!(matches!(
            not_implemented(),
            ServerError::NotImplemented("vmess over mkcp (port 19002)")
        ));
    }
}
