//! mKCP server transport — port of mihomo v1.19.34 `transport/mkcp`.
//!
//! mKCP is standard KCP ARQ plus an outer auth wrapper and optional header
//! obfuscation. The segment format uses BIG-ENDIAN u16 conversation IDs,
//! which is incompatible with off-the-shelf Rust KCP crates (u32 + little
//! endian) and with meow-transport's kcptun plugin (a different kcp-go
//! variant). This module therefore ports the format directly from the
//! golden reference in `server/zig-docs/source/transport/{kcp,mkcp}/`.

pub mod auth;
pub mod segment;
