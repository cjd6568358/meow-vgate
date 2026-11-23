//! The byte-stream currency passed between server-side transport layers.
//!
//! Deliberately local to this crate. It is structurally equivalent to
//! ADR-0001's `meow_transport::Stream` so that a future ADR can bridge the two
//! without changing protocol code, but it does not import that crate today.

use tokio::io::{AsyncRead, AsyncWrite};

/// A duplex byte stream accepted by a server-side transport layer.
pub trait ServerStream: AsyncRead + AsyncWrite + Unpin + Send + Sync {}

impl<T> ServerStream for T where T: AsyncRead + AsyncWrite + Unpin + Send + Sync {}

/// Trait-object boundary between transport layers and protocol handshakes.
pub type DynStream = Box<dyn ServerStream>;
