//! Server-side inbound stack for meow-vgate (VLESS / VMess listeners).
//!
//! # Why a separate workspace
//!
//! meow-rs is a client-only kernel. Its root workspace intentionally contains
//! no listener-side proxy protocols, and its aarch64-musl minimal build is
//! already within ~175 KiB of the 8 MiB cap. This crate therefore lives in its
//! own Cargo workspace under `server/`: the root `cargo build --workspace`
//! cannot see it, and adding inbounds here never changes the client binary.
//!
//! Run all checks from `server/`, not from the repository root.
//!
//! # Target matrix
//!
//! Deployment targets are `aarch64-unknown-linux-musl` and
//! `armv7-unknown-linux-gnueabihf` (hard-float). The 8 MiB aarch64-musl cap
//! belongs to the client binary and is untouched by this workspace; a server
//! image needs its own size ADR before release. Keeping this crate free of
//! boring-sys / OpenSSL means type-level cross checks need no C toolchain.
//!
//! # ADR-0001 and the missing `accept()`
//!
//! [`meow_transport::Transport`](https://docs.rs/meow-transport) exposes only
//! `connect()`. Its crate invariants explicitly forbid server-side code in
//! `src/`, and its dependency direction forbids taking on this crate. Rather
//! than weakening that contract, this crate defines its own symmetric
//! [`transport::ServerTransport`] with `accept(inner) -> stream`. The two
//! trains stay independent; any future convergence needs a new ADR signed by
//! the maintainer.
//!
//! # Behaviour baseline
//!
//! Wire behaviour is calibrated against `server/zig-docs/go-baseline/` (mihomo
//! v1.19.34 / `0455d0d2`), with `server/zig-docs/source/listener/{vless,vmess}/`
//! as the Zig-side cross-check. That snapshot is **read-only reference**, not a
//! buildable project and not a progress ledger. Where it disagrees with the
//! meow-rs mainline baseline, the deployment contract wins.
//!
//! # Scope, honestly
//!
//! Implemented: VLESS request-header decoding, UUID authentication, WebSocket
//! accept with path validation, and the 19001 accept loop up to a session
//! handler boundary. VMess/mKCP (19002) is a typed placeholder — see
//! [`vmess`] — and no relay/tunnel routing is wired in yet.

pub mod config;
pub mod error;
pub mod listener;
pub mod mkcp;
pub mod stream;
pub mod transport;
pub mod vless;
pub mod vmess;

pub use error::{Result, ServerError};
pub use stream::{DynStream, ServerStream};
