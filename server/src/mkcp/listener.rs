//! mKCP server listener — port of mihomo v1.19.34 `transport/mkcp/listener.go`.
//!
//! Binds a UDP socket, unwraps the auth layer and header on each datagram,
//! dispatches segments to per-session KCP connections keyed by
//! (peer IP, peer port, conversation), and drives every connection's TTI
//! tick. Newly established sessions are yielded through `accept()`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};

use crate::mkcp::auth::{self, Security};
use crate::mkcp::conn::{KcpConfig, KcpConnection, State};
use crate::mkcp::segment::{self, Segment};

/// Session key: (peer IP u32 << 32) | (peer port << 16) | conv u16, matching
/// the Zig reference's `sessionID` packing.
fn session_key(peer: SocketAddr, conv: u16) -> u64 {
    let ip = match peer.ip() {
        std::net::IpAddr::V4(ip) => u32::from(ip),
        // IPv4-mapped fallback: mihomo's mKCP listener is IPv4 in this build.
        std::net::IpAddr::V6(ip) => ip.to_ipv4().map(u32::from).unwrap_or(0),
    };
    ((ip as u64) << 32) | ((peer.port() as u64) << 16) | conv as u64
}

struct Session {
    conn: Arc<StdMutex<KcpConnection>>,
    peer: SocketAddr,
}

/// Configuration for an mKCP inbound listener.
#[derive(Debug, Clone)]
pub struct ListenerConfig {
    pub security: Security,
    pub kcp: KcpConfig,
    pub tti: Duration,
}

/// An accepted mKCP session, surfaced as a duplex byte stream. Dropping the
/// stream lets the pump task notice EOF and retire the session.
pub struct KcpStream {
    io: DuplexStream,
}

impl AsyncRead for KcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for KcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// Server-side mKCP listener. Spawn `run()` and consume `accept()`.
pub struct MkcpListener {
    socket: Arc<UdpSocket>,
    config: ListenerConfig,
    sessions: Arc<Mutex<HashMap<u64, Session>>>,
    accepted_tx: mpsc::Sender<KcpStream>,
    accepted_rx: Mutex<mpsc::Receiver<KcpStream>>,
}

impl MkcpListener {
    pub fn new(socket: UdpSocket, config: ListenerConfig) -> Self {
        let (accepted_tx, accepted_rx) = mpsc::channel(64);
        Self {
            socket: Arc::new(socket),
            config,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            accepted_tx,
            accepted_rx: Mutex::new(accepted_rx),
        }
    }

    /// Wait for the next newly established session as a byte stream.
    pub async fn accept(&self) -> Option<KcpStream> {
        self.accepted_rx.lock().await.recv().await
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Handle one incoming UDP datagram: strip auth, parse segments, dispatch
    /// to an existing or new session.
    async fn on_datagram(&self, data: &[u8], peer: SocketAddr, now_ms: u32) {
        // Deployment config has no header obfuscation or seed, so the wire is
        // just auth-wrapped segments. Non-simple security would need a nonce
        // prefix, which is handled by auth::open with an empty nonce only for
        // the simple variant.
        if !matches!(self.config.security, Security::Simple) {
            return; // seed-based nonce framing not wired for this deployment
        }
        let Some(plain) = auth::open(&self.config.security, &[], data) else {
            tracing::debug!(
                "mkcp: auth open failed for {} bytes from {peer}",
                data.len()
            );
            return;
        };
        let segments = segment::parse_all(&plain);
        if segments.is_empty() {
            tracing::debug!("mkcp: no segments parsed from {} plain bytes", plain.len());
            return;
        }
        let conv = segments[0].conv();
        let key = session_key(peer, conv);
        let mut sessions = self.sessions.lock().await;
        use std::collections::hash_map::Entry;
        let session = match sessions.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                tracing::info!(
                    "mkcp: new session conv={conv} cmd={:?} from {peer} ({} segments)",
                    segments[0].command(),
                    segments.len()
                );
                if segments[0].command() == segment::Command::Terminate {
                    return;
                }
                // The conversation ID comes from the first wire segment, not
                // from config (which is conv=0 for a listener). Using 0 here
                // would make KcpConnection::input reject every segment.
                let mut kcp_config = self.config.kcp.clone();
                kcp_config.conv = conv;
                let connection = Arc::new(StdMutex::new(KcpConnection::new(kcp_config)));
                let (client_io, pump_io) = tokio::io::duplex(64 * 1024);
                spawn_pump(Arc::clone(&connection), pump_io, self.config.tti);
                let _ = self.accepted_tx.try_send(KcpStream { io: client_io });
                entry.insert(Session {
                    conn: connection,
                    peer,
                })
            }
        };
        let mut conn = session.conn.lock().expect("kcp mutex poisoned");
        conn.input(&segments, now_ms);
        let terminated = conn.state() == State::Terminated;
        drop(conn);
        if terminated {
            sessions.remove(&key);
        }
    }

    /// Run the receive loop and the per-session TTI timer until the socket
    /// closes. Usually spawned with `tokio::spawn`.
    pub async fn run(self: Arc<Self>) -> std::io::Result<()> {
        let tti = self.config.tti;
        let security = self.config.security.clone();
        let sessions = Arc::clone(&self.sessions);
        let socket = Arc::clone(&self.socket);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tti);
            loop {
                interval.tick().await;
                let now_ms = now_millis();
                let mut outgoing = Vec::new();
                let mut guard = sessions.lock().await;
                let mut remove = Vec::new();
                for (key, session) in guard.iter() {
                    let mut conn = session.conn.lock().expect("kcp mutex poisoned");
                    let segments = conn.tick(now_ms);
                    let terminated = conn.state() == State::Terminated;
                    drop(conn);
                    if !segments.is_empty() {
                        outgoing.push((session.peer, segments));
                    }
                    if terminated {
                        remove.push(*key);
                    }
                }
                for key in remove {
                    guard.remove(&key);
                }
                drop(guard);
                for (peer, segments) in outgoing {
                    send_segments(&socket, &security, segments, peer);
                }
            }
        });

        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let (length, peer) = self.socket.recv_from(&mut buffer).await?;
            self.on_datagram(&buffer[..length], peer, now_millis())
                .await;
        }
    }
}

/// Bridge a KCP connection to a byte stream.
///
/// - Data written by the upper layer (`pump_io` readable) is queued into the
///   KCP sending window via `conn.write`.
/// - Reassembled KCP data is read on each TTI beat and written to `pump_io`
///   for the upper layer to consume via [`KcpStream`].
fn spawn_pump(conn: Arc<StdMutex<KcpConnection>>, mut pump_io: DuplexStream, tti: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(tti);
        let mut inbound = [0u8; 64 * 1024];
        let mut outbound = [0u8; 64 * 1024];
        loop {
            tokio::select! {
                read = pump_io.read(&mut inbound) => {
                    match read {
                        Ok(0) | Err(_) => break, // upper layer closed
                        Ok(n) => {
                            tracing::debug!("mkcp pump: {n} bytes stream -> conn.write");
                            let mut guard = conn.lock().expect("kcp mutex poisoned");
                            guard.write(&inbound[..n]);
                        }
                    }
                }
                _ = ticker.tick() => {
                    let n = {
                        let mut guard = conn.lock().expect("kcp mutex poisoned");
                        guard.read(&mut outbound)
                    };
                    if n > 0 {
                        tracing::debug!("mkcp pump: {n} bytes -> stream");
                        if pump_io.write_all(&outbound[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
}

/// Serialize, authenticate and send KCP segments as UDP datagrams.
fn send_segments(
    socket: &UdpSocket,
    security: &Security,
    segments: Vec<Segment>,
    peer: SocketAddr,
) {
    for segment in segments {
        let mut plain = Vec::new();
        segment.serialize(&mut plain);
        let (sealed, wire) = auth::seal(security, &[], &plain);
        let _ = socket.try_send_to(&sealed[..wire], peer);
    }
}

fn now_millis() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kcp_stream_satisfies_server_stream_contract() {
        fn assert_server_stream<T: crate::stream::ServerStream>() {}
        assert_server_stream::<KcpStream>();
    }

    #[test]
    fn session_key_is_distinct_per_peer_and_conv() {
        let a: SocketAddr = "10.0.0.1:1234".parse().unwrap();
        let b: SocketAddr = "10.0.0.2:1234".parse().unwrap();
        let c: SocketAddr = "10.0.0.1:1235".parse().unwrap();
        assert_ne!(session_key(a, 1), session_key(b, 1));
        assert_ne!(session_key(a, 1), session_key(c, 1));
        assert_ne!(session_key(a, 1), session_key(a, 2));
    }

    #[test]
    fn simple_auth_roundtrip_for_listener_datagram() {
        // A datagram sealed with the deployment security must open cleanly.
        let payload = b"kcp segment bytes";
        let (sealed, wire) = auth::seal(&Security::Simple, &[], payload);
        assert_eq!(
            auth::open(&Security::Simple, &[], &sealed[..wire]).unwrap(),
            payload
        );
    }
}

#[cfg(test)]
mod split_probe_tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn duplex_split_after_partial_read_keeps_remainder() {
        // Simulate spawn_pump -> KcpStream(duplex) -> VMess handler.
        // `pump_io` is the pump's end; `stream` is the handler's end.
        let (mut pump_io, mut stream) = tokio::io::duplex(4096);

        // Pump writes header(109)+body(97)=206 bytes in one write_all.
        let mut wire = vec![0u8; 109];
        wire.extend_from_slice(&[0xAA; 97]);
        pump_io.write_all(&wire).await.unwrap();

        // Handler reads exactly the header (109 bytes) from its end.
        let mut header = [0u8; 109];
        stream.read_exact(&mut header).await.unwrap();

        // Then splits the stream and reads the rest from the read half.
        let (mut r, _w) = tokio::io::split(stream);
        let mut rest = [0u8; 97];
        let n = r.read(&mut rest).await.unwrap();
        assert_eq!(n, 97, "split read half must see the 97 body bytes");
        assert!(rest.iter().all(|&x| x == 0xAA));
    }
}
