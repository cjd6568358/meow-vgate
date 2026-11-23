//! FORK DELTA (meow-vgate): VMess server inbound over mKCP.
//!
//! Authenticates the VMess AEAD request header (reusing meow-proxy's
//! golden-vector-locked open path), emits the response header, and relays
//! the decrypted body into the shared tunnel. Response bodies are
//! re-encrypted with the response keys the client derives independently.

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use meow_common::{ConnType, Metadata, Network};
use meow_proxy::vmess::body::BodyCipher;
use meow_proxy::vmess::header::{
    cmd_key, seal_response_header, server_open_request_header, Security,
};
use meow_server::mkcp::auth::Security as MkcpSecurity;
use meow_server::mkcp::conn::KcpConfig;
use meow_server::mkcp::listener::{KcpStream, ListenerConfig, MkcpListener};
use smol_str::SmolStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;

use crate::server_inbound::RouteSink;

const ADDR_IPV4: u8 = 0x01;
const ADDR_DOMAIN: u8 = 0x02;
const ADDR_IPV6: u8 = 0x03;
const CMD_TCP: u8 = 0x01;
const CMD_UDP: u8 = 0x02;

/// Parsed VMess request header (plaintext after AEAD open).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmessRequest {
    pub req_key: [u8; 16],
    pub req_iv: [u8; 16],
    pub resp_v: u8,
    pub security: Security,
    pub command: u8,
    pub dst_ip: Option<IpAddr>,
    pub host: String,
    pub port: u16,
}

/// Parse the AEAD-opened plaintext header.
///
/// Layout: `[0]=1 [1..17]=reqIV [17..33]=reqKey [33]=respV [34]=opt
/// [35]=padding<<4|security [36]=0 [37]=cmd [38..40]=port BE [40]=atype
/// [41..]=addr [..]=padding [last4]=FNV1a32`.
pub fn parse_plaintext_header(plain: &[u8]) -> Result<VmessRequest> {
    if plain.len() < 42 {
        return Err(anyhow!("vmess header too short: {}", plain.len()));
    }
    if plain[0] != 0x01 {
        return Err(anyhow!("unsupported vmess header version: {}", plain[0]));
    }
    let req_iv: [u8; 16] = plain[1..17]
        .try_into()
        .map_err(|_| anyhow!("short req_iv"))?;
    let req_key: [u8; 16] = plain[17..33]
        .try_into()
        .map_err(|_| anyhow!("short req_key"))?;
    let resp_v = plain[33];
    let security = match plain[35] & 0x0f {
        0x03 => Security::Aes128Gcm,
        0x04 => Security::ChaCha20Poly1305,
        0x05 => Security::None,
        other => return Err(anyhow!("unknown vmess security nibble: {other:#x}")),
    };
    let command = plain[37];
    let port = u16::from_be_bytes([plain[38], plain[39]]);
    let atype = plain[40];
    let (dst_ip, host) = match atype {
        ADDR_IPV4 => {
            let octets: [u8; 4] = plain
                .get(41..45)
                .ok_or_else(|| anyhow!("truncated IPv4"))?
                .try_into()
                .map_err(|_| anyhow!("ipv4"))?;
            let ip = IpAddr::V4(octets.into());
            (Some(ip), ip.to_string())
        }
        ADDR_IPV6 => {
            let octets: [u8; 16] = plain
                .get(41..57)
                .ok_or_else(|| anyhow!("truncated IPv6"))?
                .try_into()
                .map_err(|_| anyhow!("ipv6"))?;
            let ip = IpAddr::V6(octets.into());
            (Some(ip), ip.to_string())
        }
        ADDR_DOMAIN => {
            let len = plain[41] as usize;
            let bytes = plain
                .get(42..42 + len)
                .ok_or_else(|| anyhow!("truncated domain"))?;
            (None, String::from_utf8_lossy(bytes).into_owned())
        }
        other => return Err(anyhow!("unknown address type: {other:#x}")),
    };
    Ok(VmessRequest {
        req_key,
        req_iv,
        resp_v,
        security,
        command,
        dst_ip,
        host,
        port,
    })
}

/// Find the configured user whose cmd_key can open the request header.
pub fn authenticate(users: &[[u8; 16]], wire: &[u8]) -> Option<VmessRequest> {
    if wire.len() < 42 {
        return None;
    }
    for uuid in users {
        let key = cmd_key(uuid);
        if let Ok(plain) = server_open_request_header(&key, wire) {
            if let Ok(request) = parse_plaintext_header(&plain) {
                return Some(request);
            }
        }
    }
    None
}

fn metadata_for(request: &VmessRequest, sink: &RouteSink) -> Metadata {
    Metadata {
        network: if request.command == CMD_UDP {
            Network::Udp
        } else {
            Network::Tcp
        },
        conn_type: ConnType::Vmess,
        dst_ip: request.dst_ip,
        dst_port: request.port,
        host: SmolStr::from(request.host.as_str()),
        in_name: sink.inbound_name().clone(),
        in_port: sink.inbound_port(),
        ..Default::default()
    }
}

/// Serve one VMess session end to end. Generic over the byte transport so
/// tests can drive it with an in-memory duplex stream.
pub async fn handle_vmess_session<S>(
    mut stream: S,
    users: Arc<Vec<[u8; 16]>>,
    sink: RouteSink,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // 1. Read the fixed 42-byte AEAD header prefix.
    let mut prefix = [0u8; 42];
    stream
        .read_exact(&mut prefix)
        .await
        .map_err(|e| anyhow!("vmess header read: {e}"))?;
    // The AEAD header's payload follows: read enough for a typical header.
    let mut header = prefix.to_vec();
    // Peek more bytes if a larger plaintext header is present. mKCP delivers
    // a stream, so we read what is immediately available via a short read.
    let mut extra = [0u8; 512];
    // Non-blocking-ish: read with a short deadline to avoid hanging when the
    // client waits for the response header before sending body records.
    let more = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        stream.read(&mut extra),
    )
    .await
    .unwrap_or(Ok(0))
    .unwrap_or(0);
    header.extend_from_slice(&extra[..more]);

    // 2. Authenticate against the configured users.
    let request = authenticate(&users, &header).ok_or_else(|| anyhow!("vmess auth failed"))?;
    if request.command != CMD_TCP && request.command != CMD_UDP {
        return Err(anyhow!("unsupported vmess command: {}", request.command));
    }

    // 3. Emit response header so the client proceeds to send body records.
    let response_header = seal_response_header(&request.req_key, &request.req_iv, request.resp_v);
    stream
        .write_all(&response_header)
        .await
        .map_err(|e| anyhow!("vmess response header write: {e}"))?;

    // 4. Bridge decrypted plaintext to the shared tunnel via a duplex pair.
    let (tunnel_side, vmess_side) = tokio::io::duplex(64 * 1024);
    let (mut vmess_read, mut vmess_write) = tokio::io::split(vmess_side);
    let sink = sink.clone();
    let metadata = metadata_for(&request, &sink);
    let relay = tokio::spawn(async move {
        let mut tunnel_side = tunnel_side;
        sink.relay(metadata, &mut tunnel_side).await;
    });

    let mut cipher =
        BodyCipher::new_server_full(request.security, &request.req_key, &request.req_iv);
    let mut record = Vec::new();
    // 5. Pump: encrypted KCP records -> plaintext tunnel, and back.
    loop {
        // Read one encrypted request record, decrypt, forward to tunnel.
        match cipher.read_record_into(&mut stream, &mut record).await {
            Ok(Some(())) => {
                if vmess_write.write_all(&record).await.is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(anyhow!("vmess body read: {e}")),
        }
        // Drain any plaintext the tunnel wants to send back, encrypt, write.
        let mut plain = [0u8; 16 * 1024];
        match vmess_read.read(&mut plain).await {
            Ok(0) => break,
            Ok(n) => {
                cipher
                    .write_record(&mut stream, &plain[..n])
                    .await
                    .map_err(|e| anyhow!("vmess body write: {e}"))?;
            }
            Err(e) => return Err(anyhow!("vmess tunnel read: {e}")),
        }
    }
    relay.abort();
    Ok(())
}

/// Serve a VMess-over-mKCP listener into the shared tunnel.
pub async fn serve_vmess_mkcp(
    socket: UdpSocket,
    users: Arc<Vec<[u8; 16]>>,
    sink: RouteSink,
    config: KcpConfig,
    tti: std::time::Duration,
) -> Result<()> {
    let listener = Arc::new(MkcpListener::new(
        socket,
        ListenerConfig {
            security: MkcpSecurity::Simple,
            kcp: config,
            tti,
        },
    ));
    let runner = Arc::clone(&listener);
    tokio::spawn(async move {
        if let Err(e) = runner.run().await {
            tracing::error!("mKCP listener error: {e}");
        }
    });
    while let Some(stream) = listener.accept().await {
        let users = Arc::clone(&users);
        let sink = sink.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_vmess_session(stream, users, sink).await {
                tracing::warn!("vmess session error: {e:#}");
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plaintext_header_ipv4() {
        let mut plain = vec![0u8; 42 + 4 + 4];
        plain[0] = 0x01;
        plain[1..17].copy_from_slice(&[0x10; 16]);
        plain[17..33].copy_from_slice(&[0x20; 16]);
        plain[33] = 0x77;
        plain[35] = 0x03;
        plain[37] = CMD_TCP;
        plain[38..40].copy_from_slice(&443u16.to_be_bytes());
        plain[40] = ADDR_IPV4;
        plain[41..45].copy_from_slice(&[1, 1, 1, 1]);
        let request = parse_plaintext_header(&plain).expect("valid header");
        assert_eq!(request.port, 443);
        assert_eq!(request.security, Security::Aes128Gcm);
        assert_eq!(request.resp_v, 0x77);
        assert_eq!(request.host, "1.1.1.1");
        assert_eq!(request.req_key, [0x20; 16]);
    }

    #[test]
    fn parses_plaintext_header_domain() {
        let domain = b"example.com";
        let mut plain = vec![0u8; 42 + 1 + domain.len() + 4];
        plain[0] = 0x01;
        plain[35] = 0x04;
        plain[37] = CMD_TCP;
        plain[38..40].copy_from_slice(&80u16.to_be_bytes());
        plain[40] = ADDR_DOMAIN;
        plain[41] = domain.len() as u8;
        plain[42..42 + domain.len()].copy_from_slice(domain);
        let request = parse_plaintext_header(&plain).expect("valid header");
        assert_eq!(request.host, "example.com");
        assert_eq!(request.port, 80);
        assert_eq!(request.security, Security::ChaCha20Poly1305);
        assert_eq!(request.dst_ip, None);
    }

    #[test]
    fn rejects_bad_version() {
        assert!(parse_plaintext_header(&[0x02; 50]).is_err());
    }

    #[test]
    fn client_sealed_header_authenticates() {
        // Mirror what a conformant client (mihomo/FlClash) emits: seal with
        // meow-proxy's client path, then open with our server path.
        let uuid = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ];
        let key = cmd_key(&uuid);
        let metadata = Metadata {
            host: SmolStr::from("example.com"),
            dst_port: 443,
            ..Default::default()
        };
        let sealed = meow_proxy::vmess::header::seal_request_header(
            &key,
            Security::Aes128Gcm,
            &metadata,
            false,
        )
        .expect("client seal");
        let opened = authenticate(&[uuid], &sealed.bytes).expect("server must open client header");
        assert_eq!(opened.host, "example.com");
        assert_eq!(opened.port, 443);
        assert_eq!(opened.command, CMD_TCP);
        assert_eq!(opened.security, Security::Aes128Gcm);
        assert_eq!(opened.req_key, sealed.req_key);
        assert_eq!(opened.req_iv, sealed.req_iv);
        assert_eq!(opened.resp_v, sealed.resp_v);
    }
}
