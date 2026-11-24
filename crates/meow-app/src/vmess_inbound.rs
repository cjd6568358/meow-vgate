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
use meow_server::mkcp::listener::{ListenerConfig, MkcpListener};
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
    tracing::info!("vmess: session accepted, awaiting AEAD header");
    // 1. Read the fixed 42-byte AEAD header prefix (auth_id + sealed length +
    // conn_nonce). Try each configured user's cmd_key to decrypt the length.
    let mut prefix = [0u8; 42];
    stream
        .read_exact(&mut prefix)
        .await
        .map_err(|e| anyhow!("vmess header prefix read: {e}"))?;
    let frame_len = users
        .iter()
        .find_map(|uuid| {
            meow_proxy::vmess::header::request_header_frame_len(&cmd_key(uuid), &prefix).ok()
        })
        .ok_or_else(|| anyhow!("vmess header length AEAD open failed"))?;
    if !(58..=64 * 1024).contains(&frame_len) {
        return Err(anyhow!(
            "implausible vmess header frame length: {frame_len}"
        ));
    }

    // 2. Read exactly the remaining sealed payload, then authenticate.
    let mut header = prefix.to_vec();
    header.resize(frame_len, 0);
    stream
        .read_exact(&mut header[42..])
        .await
        .map_err(|e| anyhow!("vmess header payload read: {e}"))?;
    let request = authenticate(&users, &header).ok_or_else(|| {
        tracing::warn!("vmess: auth failed for {} users", users.len());
        anyhow!("vmess auth failed")
    })?;
    tracing::info!(
        "vmess: authenticated cmd={} -> {}:{}",
        request.command,
        request.host,
        request.port
    );
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
    let (vmess_read, vmess_write) = tokio::io::split(vmess_side);
    let sink = sink.clone();
    let metadata = metadata_for(&request, &sink);
    let mut relay = tokio::spawn(async move {
        let mut tunnel_side = tunnel_side;
        sink.relay(metadata, &mut tunnel_side).await;
        tracing::debug!("vmess body: relay task finished");
    });

    // 5. Two independent pumps. VMess is full-duplex: the client may send a
    // request body and wait for the response before sending anything more.
    // A single read-then-write loop would deadlock in that case, so split the
    // stream and run each direction in its own task.
    let (mut kcp_read, mut kcp_write) = tokio::io::split(stream);

    // Inbound: encrypted KCP records -> decrypted plaintext -> tunnel.
    let security = request.security;
    let req_key = request.req_key;
    let req_iv = request.req_iv;
    let inbound = tokio::spawn(async move {
        tracing::debug!("vmess body: inbound pump started");
        let mut cipher = BodyCipher::new_server_full(security, &req_key, &req_iv);

        let mut record = Vec::new();
        let mut writer = vmess_write;
        loop {
            match cipher.read_record_into(&mut kcp_read, &mut record).await {
                Ok(Some(())) => {
                    tracing::debug!("vmess body: decrypted {} bytes -> tunnel", record.len());
                    if writer.write_all(&record).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("vmess body: read_record error: {e}");
                    break;
                }
            }
        }
        tracing::debug!("vmess body: inbound pump ended");
    });

    // Outbound: tunnel plaintext -> encrypted records -> KCP peer.
    let outbound = tokio::spawn(async move {
        let mut cipher = BodyCipher::new_server_full(security, &req_key, &req_iv);
        let mut reader = vmess_read;
        let mut plain = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut plain).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tracing::debug!("vmess body: {n} bytes tunnel -> encrypted");
                    if cipher
                        .write_record(&mut kcp_write, &plain[..n])
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    // Keep the relay alive until either direction ends, then tear down.
    tracing::debug!("vmess: entering select (frame_len={frame_len})");
    tokio::select! {
        r = &mut relay => { tracing::debug!("vmess: select relay ended: {:?}", r); },
        r = inbound => { tracing::debug!("vmess: select inbound ended: {:?}", r); },
        r = outbound => { tracing::debug!("vmess: select outbound ended: {:?}", r); },
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
        // The frame length helper MUST return the exact header wire length,
        // otherwise the handler reads too few/many bytes and body framing
        // starts at the wrong offset (seen as garbage record lengths e2e).
        let frame_len =
            meow_proxy::vmess::header::request_header_frame_len(&key, &sealed.bytes[..42])
                .expect("frame len");
        assert_eq!(
            frame_len,
            sealed.bytes.len(),
            "frame_len must equal full header wire length"
        );
        let opened = authenticate(&[uuid], &sealed.bytes).expect("server must open client header");
        assert_eq!(opened.host, "example.com");
        assert_eq!(opened.port, 443);
        assert_eq!(opened.command, CMD_TCP);
        assert_eq!(opened.security, Security::Aes128Gcm);
        assert_eq!(opened.req_key, sealed.req_key);
        assert_eq!(opened.req_iv, sealed.req_iv);
        assert_eq!(opened.resp_v, sealed.resp_v);
    }

    #[tokio::test]
    async fn body_cipher_client_seal_server_open_roundtrip() {
        let req_key = [0x20u8; 16];
        let req_iv = [0x10u8; 16];
        let plaintext = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";

        // Client seals a request-body record with the request keys.
        let mut writer = BodyCipher::new_writer(Security::Aes128Gcm, &req_key, &req_iv);
        let mut wire = Vec::new();
        writer.write_record(&mut wire, plaintext).await.unwrap();
        let len_prefix = u16::from_be_bytes([wire[0], wire[1]]) as usize;
        assert_eq!(len_prefix, plaintext.len() + 16);

        // Server opens with the full directional cipher.
        let mut server = BodyCipher::new_server_full(Security::Aes128Gcm, &req_key, &req_iv);
        let mut cursor = &wire[..];
        let mut out = Vec::new();
        let got = server
            .read_record_into(&mut cursor, &mut out)
            .await
            .unwrap();
        assert!(got.is_some());
        assert_eq!(&out, plaintext);
    }
}
