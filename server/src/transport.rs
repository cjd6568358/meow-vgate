//! Server-side transport layers — the symmetric counterpart of ADR-0001.
//!
//! ADR-0001 defines `Transport::connect(inner) -> stream` for outbounds and
//! leaves `accept()` as an open question. This crate answers that question for
//! inbounds without touching `meow-transport`, whose crate invariants forbid
//! server code in `src/` and forbid depending on other meow crates.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bytes::Bytes;
use futures_util::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{self, StatusCode};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_hdr_async, WebSocketStream};

use crate::error::Result;
use crate::stream::DynStream;

/// A transport layer that unwraps an inbound `inner` stream.
///
/// This is `Transport::connect` mirrored: outbounds wrap upward from a TCP
/// stream; inbounds unwrap downward until the protocol handshake sees plain
/// bytes. Layers compose the same way as their client-side counterparts.
#[async_trait]
pub trait ServerTransport: Send + Sync {
    async fn accept(&self, inner: DynStream) -> Result<DynStream>;
}

/// WebSocket accept layer (VLESS `network: ws`, port 19001).
///
/// The upgrade and frame codec are delegated to tungstenite, matching the
/// client-side `meow-transport::ws` dependency choice. Only the request path is
/// validated here; TLS, when needed, is an outer layer and is not wired yet.
#[derive(Debug, Clone)]
pub struct WsAccept {
    path: String,
}

impl WsAccept {
    /// Build an accept layer that only serves upgrades on `path`.
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl ServerTransport for WsAccept {
    // tungstenite's `Callback` trait fixes the error type to a by-value
    // `http::Response<Option<String>>`; boxing it is not an option here.
    #[allow(clippy::result_large_err)]
    async fn accept(&self, inner: DynStream) -> Result<DynStream> {
        let path = self.path.clone();
        // Xray/v2ray/mihomo VLESS-WS clients may carry the first application
        // bytes (the VLESS request header + initial payload) base64url-encoded
        // in Sec-WebSocket-Protocol (0-RTT). The handshake response does NOT
        // echo it (matching go-baseline/transport/vmess/websocket.go); instead
        // the decoded bytes must be fed to the VLESS reader BEFORE WS frames.
        let early = Arc::new(std::sync::Mutex::new(Vec::new()));
        let early_cb = Arc::clone(&early);
        let websocket: WebSocketStream<DynStream> = accept_hdr_async(
            inner,
            move |request: &Request,
                  response: Response|
                  -> std::result::Result<Response, ErrorResponse> {
                // tungstenite supplies the Sec-WebSocket-Accept header; keep
                // its response and only add the path check.
                let response = validate_response_path(request, response, &path)?;
                if let Some(value) = request.headers().get("sec-websocket-protocol") {
                    if let Ok(text) = value.to_str() {
                        if let Some(decoded) = decode_early_data(text) {
                            *early_cb.lock().expect("early-data mutex poisoned") = decoded;
                        }
                    }
                }
                Ok(response)
            },
        )
        .await?;

        let early = early.lock().expect("early-data mutex poisoned").clone();
        Ok(Box::new(WsByteStream::new_with_early_data(
            websocket, early,
        )))
    }
}

/// Keep the supplied tungstenite response intact and reject on path mismatch.
///
/// Factored out so the check is testable without opening a socket.
// Same externally imposed error type as `accept` above.
#[allow(clippy::result_large_err)]
fn validate_response_path(
    request: &Request,
    response: Response,
    path: &str,
) -> std::result::Result<Response, ErrorResponse> {
    if request.uri().path() != path {
        let body = format!("expected websocket path {path:?}");
        return Err(http::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Some(body))
            .expect("a fixed status and body always build a valid response"));
    }
    Ok(response)
}

/// Decode Xray/v2ray WS 0-RTT early data from Sec-WebSocket-Protocol.
/// go-baseline/transport/vmess/websocket.go:529 maps `+`/`/`→`-`/`_` and
/// strips `=`, i.e. base64url without padding. Returns None for a header that
/// is not valid early data (e.g. a genuine subprotocol negotiation string).
fn decode_early_data(value: &str) -> Option<Vec<u8>> {
    if value.is_empty() {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(value).ok()?;
    (!decoded.is_empty()).then_some(decoded)
}

/// Byte-oriented view of a WebSocket message stream.
///
/// `tokio-tungstenite 0.29` exposes `Stream<Message>` / `Sink<Message>` but no
/// `AsyncRead`/`AsyncWrite`, so this adapter maps binary frames to bytes.
/// Ping replies are tungstenite's own job (it queues a Pong when a Ping is
/// read and flushes it on the next read/write — the docs say not to answer
/// manually, and this adapter no longer duplicates it). It ignores Pongs
/// and turns Close/EOF into a clean read EOF.
pub struct WsByteStream<S> {
    inner: WebSocketStream<S>,
    read_buffer: Bytes,
    read_offset: usize,
}

impl<S> WsByteStream<S> {
    /// Seed the read buffer with WS 0-RTT early data (decoded from the
    /// client's Sec-WebSocket-Protocol). poll_read drains this before any
    /// real WS frame, so VLESS sees the request header first.
    fn new_with_early_data(inner: WebSocketStream<S>, early: Vec<u8>) -> Self {
        Self {
            inner,
            read_buffer: Bytes::from(early),
            read_offset: 0,
        }
    }
}

impl<S> AsyncRead for WsByteStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        target: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if target.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        if this.read_offset < this.read_buffer.len() {
            let remaining = &this.read_buffer[this.read_offset..];
            let count = remaining.len().min(target.remaining());
            target.put_slice(&remaining[..count]);
            this.read_offset += count;
            return Poll::Ready(Ok(()));
        }

        loop {
            match Pin::new(&mut this.inner).poll_next(context) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Err(io::Error::other(error)));
                }
                Poll::Ready(Some(Ok(message))) => match message {
                    Message::Binary(bytes) => {
                        this.read_buffer = bytes;
                        this.read_offset = 0;
                        if this.read_buffer.is_empty() {
                            continue;
                        }
                        let count = this.read_buffer.len().min(target.remaining());
                        target.put_slice(&this.read_buffer[..count]);
                        this.read_offset = count;
                        return Poll::Ready(Ok(()));
                    }
                    Message::Ping(_) => {
                        // tungstenite queues the Pong reply itself and
                        // flushes it on the next read/write — answering
                        // manually here used to send a duplicate Pong.
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) | Message::Frame(_) => {
                        return Poll::Ready(Ok(()));
                    }
                    Message::Text(_) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "VLESS over WS requires binary frames",
                        )));
                    }
                },
            }
        }
    }
}

impl<S> AsyncWrite for WsByteStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let mut sink = Pin::new(&mut this.inner);
        match sink.as_mut().poll_ready(context) {
            Poll::Ready(Ok(())) => {
                let length = bytes.len();
                sink.start_send(Message::Binary(Bytes::copy_from_slice(bytes)))
                    .map_err(io::Error::other)?;
                Poll::Ready(Ok(length))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(io::Error::other(error))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner)
            .poll_flush(context)
            .map_err(io::Error::other)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner)
            .poll_close(context)
            .map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::http::Request;

    fn request(path: &str) -> Request<()> {
        Request::builder()
            .method("GET")
            .uri(path)
            .body(())
            .expect("fixed request builds")
    }

    fn response() -> Response {
        Response::new(())
    }

    #[test]
    fn accepts_the_configured_path() {
        let kept =
            validate_response_path(&request("/ws"), response(), "/ws").expect("matching path");
        assert_eq!(kept.status(), StatusCode::OK);
    }

    #[test]
    fn rejects_a_different_path() {
        let error =
            validate_response_path(&request("/other"), response(), "/ws").expect_err("bad path");
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn path_check_is_exact_not_prefix_based() {
        assert!(validate_response_path(&request("/ws/extra"), response(), "/ws").is_err());
        assert!(validate_response_path(&request("/WS"), response(), "/ws").is_err());
    }

    #[test]
    fn decodes_xray_ws_0rtt_early_data() {
        // Captured shape from a real mihomo/FlClash client: VLESS header bytes
        // encoded as base64url without padding in Sec-WebSocket-Protocol.
        let encoded = URL_SAFE_NO_PAD.encode(b"\x00\x01\x02VLESS-payload");
        assert_eq!(
            decode_early_data(&encoded),
            Some(b"\x00\x01\x02VLESS-payload".to_vec())
        );
    }

    #[test]
    fn rejects_empty_or_invalid_early_data() {
        assert_eq!(decode_early_data(""), None);
        // '!' is outside base64url; Go's decodeEd fails the same way and
        // returns no early data (falls back to reading WS frames).
        assert_eq!(decode_early_data("not-base64!"), None);
    }
}
