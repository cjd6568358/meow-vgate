//! Server-side transport layers — the symmetric counterpart of ADR-0001.
//!
//! ADR-0001 defines `Transport::connect(inner) -> stream` for outbounds and
//! leaves `accept()` as an open question. This crate answers that question for
//! inbounds without touching `meow-transport`, whose crate invariants forbid
//! server code in `src/` and forbid depending on other meow crates.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
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
        let websocket: WebSocketStream<DynStream> = accept_hdr_async(
            inner,
            move |request: &Request,
                  response: Response|
                  -> std::result::Result<Response, ErrorResponse> {
                // tungstenite supplies the Sec-WebSocket-Accept header; keep
                // its response and only add the path check.
                validate_response_path(request, response, &path)
            },
        )
        .await?;

        Ok(Box::new(WsByteStream::new(websocket)))
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

/// Byte-oriented view of a WebSocket message stream.
///
/// `tokio-tungstenite 0.29` exposes `Stream<Message>` / `Sink<Message>` but no
/// `AsyncRead`/`AsyncWrite`, so this adapter maps binary frames to bytes. It
/// answers Pings, ignores Pongs, and turns Close/EOF into a clean read EOF.
pub struct WsByteStream<S> {
    inner: WebSocketStream<S>,
    read_buffer: Bytes,
    read_offset: usize,
    pending_pong: Option<Bytes>,
}

impl<S> WsByteStream<S> {
    fn new(inner: WebSocketStream<S>) -> Self {
        Self {
            inner,
            read_buffer: Bytes::new(),
            read_offset: 0,
            pending_pong: None,
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
                    Message::Ping(payload) => {
                        this.pending_pong = Some(payload);
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
        if let Some(payload) = this.pending_pong.take() {
            let mut sink = Pin::new(&mut this.inner);
            match sink.as_mut().poll_ready(context) {
                Poll::Ready(Ok(())) => sink
                    .start_send(Message::Pong(payload))
                    .map_err(io::Error::other)?,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(io::Error::other(error))),
                Poll::Pending => {
                    this.pending_pong = Some(payload);
                    return Poll::Pending;
                }
            }
        }

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
}
