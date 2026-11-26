//! mKCP server listener — port of mihomo v1.19.34 `transport/mkcp/listener.go`.
//!
//! Binds a UDP socket, unwraps the auth layer and header on each datagram,
//! dispatches segments to per-session KCP connections keyed by
//! (peer IP, peer port, conversation), and drives every connection's TTI
//! tick. Newly established sessions are yielded through `accept()`. Outgoing
//! segments are batched whole-per-datagram up to the MTU before the auth
//! layer seals them (Go `kcp.go flush()` behaviour).

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
                // Backpressure: a full accept queue means the session
                // handler is not draining fast enough (64 pending). The
                // connection is still inserted below — the dropped stream
                // half makes the pump see read-EOF, the close ladder fires,
                // and the client receives TERMINATE instead of a hang. It
                // must never be silent though: the operator needs the
                // signal that the accept rate exceeds the handler capacity.
                if let Err(error) = self.accepted_tx.try_send(KcpStream { io: client_io }) {
                    tracing::warn!(
                        "mkcp: accept queue full — refusing new session conv={conv} \
                         from {peer} (close ladder will notify the client): {error}"
                    );
                }
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
        let mtu = self.config.kcp.mtu;
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
                    send_segments(&socket, &security, segments, peer, mtu);
                }
            }
        });

        let mut buffer = vec![0u8; 64 * 1024];
        let mut consecutive_errors: u32 = 0;
        loop {
            match self.socket.recv_from(&mut buffer).await {
                Ok((length, peer)) => {
                    consecutive_errors = 0;
                    self.on_datagram(&buffer[..length], peer, now_millis())
                        .await;
                }
                Err(error) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_RECV_ERRORS {
                        return Err(error);
                    }
                    // Transient socket errors (ENFILE/EMFILE, or Windows'
                    // WSAECONNRESET surfacing a past ICMP unreachable) must
                    // not retire the whole inbound. Go's mKCP reader stops on
                    // the first error; we retry with backoff instead (Class B
                    // divergence, registered in server/docs/adr/0002).
                    tracing::warn!(
                        "mkcp recv_from failed ({consecutive_errors} consecutive): {error}"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
}

/// Consecutive recv errors tolerated before the run loop gives up.
const MAX_CONSECUTIVE_RECV_ERRORS: u32 = 100;

/// Bridge a KCP connection to a byte stream.
///
/// - Data written by the upper layer (`pump_io` readable) is queued into the
///   KCP sending window via `conn.write`.
/// - Reassembled KCP data is read on each TTI beat and written to `pump_io`
///   for the upper layer to consume via [`KcpStream`].
///
/// Connection lifecycle: when the handler drops [`KcpStream`] this pump sees
/// read-EOF and retires the connection through `close()` — pending sends
/// flush, then TERMINATE is pinged until the peer follows — instead of
/// waiting for the 30 s idle timeout. Conversely, once the listener's tick
/// ladder reaches `Terminated` (peer TERMINATE or the timeout ladder), the
/// pump drops its duplex half so the handler sees read-EOF too.
fn spawn_pump(conn: Arc<StdMutex<KcpConnection>>, mut pump_io: DuplexStream, tti: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(tti);
        let mut inbound = [0u8; 64 * 1024];
        let mut outbound = [0u8; 64 * 1024];
        loop {
            tokio::select! {
                read = pump_io.read(&mut inbound) => {
                    match read {
                        Ok(0) | Err(_) => {
                            // Upper layer closed (handler dropped KcpStream):
                            // walk the close ladder now so the peer receives
                            // TERMINATE instead of a 30 s hang.
                            conn.lock().expect("kcp mutex poisoned").close(now_millis());
                            break;
                        }
                        Ok(n) => {
                            tracing::debug!("mkcp pump: {n} bytes stream -> conn.write");
                            let mut guard = conn.lock().expect("kcp mutex poisoned");
                            guard.write(&inbound[..n]);
                        }
                    }
                }
                _ = ticker.tick() => {
                    let (n, terminated) = {
                        let mut guard = conn.lock().expect("kcp mutex poisoned");
                        (guard.read(&mut outbound), guard.state() == State::Terminated)
                    };
                    // The tick ladder retired the connection (peer TERMINATE,
                    // or the close-timeout ladder ran out): stop pumping and
                    // drop our half so the handler's KcpStream sees EOF.
                    if terminated {
                        break;
                    }
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
///
/// Segments are packed whole-per-datagram up to the configured MTU before
/// the auth layer seals each datagram — the batching Go's `kcp.go flush()`
/// applies. One sealed datagram may therefore carry several back-to-back
/// segments (the receiver's `parse_all` handles that natively); a segment is
/// never split across datagrams, so framing survives even an absurdly small
/// MTU (a lone oversized segment sends alone instead of being torn).
fn send_segments(
    socket: &UdpSocket,
    security: &Security,
    segments: Vec<Segment>,
    peer: SocketAddr,
    mtu: u32,
) {
    for plain in pack_segment_datagrams(&segments, mtu) {
        let (sealed, wire) = auth::seal(security, &[], &plain);
        if wire == 0 {
            // Fail-closed auth guard (see mkcp::auth): nothing to send.
            // The listener never selects AES today, so this is defensive.
            tracing::warn!("mkcp: auth sealed an empty datagram, skipping");
            continue;
        }
        // Never silently drop a send failure: a UDP sendto can fail on a
        // constrained target (small socket buffer, address-family mismatch)
        // and the peer then sees a hang with no server-side evidence.
        if let Err(e) = socket.try_send_to(&sealed[..wire], peer) {
            tracing::warn!("mkcp sendto {peer} failed: {e}");
        }
    }
}

/// Serialize whole segments into plaintext datagram payloads of at most
/// `mtu` bytes each. Mirrors Go's `kcp.go flush()` accumulation buffer.
fn pack_segment_datagrams(segments: &[Segment], mtu: u32) -> Vec<Vec<u8>> {
    let mtu = mtu as usize;
    let mut datagrams = Vec::new();
    let mut current = Vec::new();
    for segment in segments {
        let mut serialized = Vec::new();
        segment.serialize(&mut serialized);
        if !current.is_empty() && current.len() + serialized.len() > mtu {
            datagrams.push(std::mem::take(&mut current));
        }
        current.extend_from_slice(&serialized);
    }
    if !current.is_empty() {
        datagrams.push(current);
    }
    datagrams
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

    use crate::mkcp::segment::DATA_OVERHEAD;

    fn data_segment(number: u32, payload: &[u8]) -> Segment {
        Segment::Data {
            conv: 1,
            option: 0,
            timestamp: 0,
            number,
            sending_next: 0,
            payload: payload.to_vec(),
        }
    }

    // Go kcp.go flush() batches segments into MTU-sized datagrams; the
    // pre-batching code sent one datagram per segment (~46 B header +
    // one syscall each) — the 2026-11-26 review's Class B divergence #5.
    #[test]
    fn pack_batches_whole_segments_up_to_the_mtu() {
        let segments: Vec<Segment> = (0..8u32).map(|i| data_segment(i, &[0xA5; 100])).collect();
        let mtu = 1350u32;
        let datagrams = pack_segment_datagrams(&segments, mtu);
        let per_segment = DATA_OVERHEAD + 100;
        let expected = (per_segment * 8).div_ceil(mtu as usize);
        assert!(datagrams.len() < 8, "small segments must be batched");
        assert_eq!(datagrams.len(), expected);
        for datagram in &datagrams {
            assert!(datagram.len() <= mtu as usize, "datagram exceeds mtu");
        }
        // Every segment must survive unpacking, in order and whole.
        let mut flat = Vec::new();
        for datagram in &datagrams {
            flat.extend(segment::parse_all(datagram));
        }
        assert_eq!(flat.len(), 8);
        for (i, seg) in flat.iter().enumerate() {
            match seg {
                Segment::Data {
                    number, payload, ..
                } => {
                    assert_eq!(*number, i as u32);
                    assert_eq!(payload.len(), 100);
                }
                other => panic!("expected DATA, got {other:?}"),
            }
        }
    }

    #[test]
    fn pack_keeps_an_mtusized_segment_alone() {
        // A segment exactly the size of the MTU fills its datagram; the
        // next segment must start a new datagram instead of overflowing.
        let mss = 100usize;
        let mtu = (DATA_OVERHEAD + mss) as u32;
        let segments = vec![data_segment(0, &vec![1u8; mss]), data_segment(1, &[2; 5])];
        let datagrams = pack_segment_datagrams(&segments, mtu);
        assert_eq!(datagrams.len(), 2);
        assert_eq!(datagrams[0].len(), mtu as usize);
        assert!(datagrams[1].len() <= mtu as usize);
        // Both survive unpacking.
        let flat: Vec<Segment> = datagrams
            .iter()
            .flat_map(|d| segment::parse_all(d))
            .collect();
        assert_eq!(flat.len(), 2);
    }

    // The handler finished and dropped KcpStream: the connection must be
    // retired through the close ladder (TERMINATE back to the peer) instead
    // of idling until the 30 s timeout — the pre-fix behaviour hung the peer
    // with no server-side evidence (2026-11-26 review, P1).
    //
    // Runs on a multi_thread runtime (like the production `#[tokio::main]`),
    // NOT the default current-thread test runtime: under the parallel test
    // swarm the spawned pump task was observed (tokio 1.54, Windows) to
    // never be polled again after its first immediate tick — the close
    // ladder then never fired and this test flaked at ~4–15%. The driver-
    // pinned single-thread scheduler was the common factor; two worker
    // threads did not reproduce it in hundreds of runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_session_stream_terminates_the_connection() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("ephemeral UDP port");
        let server_addr = socket.local_addr().unwrap();
        let config = ListenerConfig {
            security: Security::Simple,
            // Small TTI so the close ladder runs within milliseconds.
            kcp: KcpConfig::from_inbound(0, 0, 20, 0, 0, 0),
            tti: Duration::from_millis(20),
        };
        let listener = Arc::new(MkcpListener::new(socket, config));
        let runner = Arc::clone(&listener);
        tokio::spawn(async move {
            let _ = runner.run().await;
        });

        // Client side: one authenticated DATA segment establishes a session.
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("client UDP port");
        let mut data = Vec::new();
        Segment::Data {
            conv: 0xBEEF,
            option: 0,
            timestamp: 0,
            number: 0,
            sending_next: 0,
            payload: b"hello".to_vec(),
        }
        .serialize(&mut data);
        let (sealed, wire) = auth::seal(&Security::Simple, &[], &data);
        client
            .send_to(&sealed[..wire], server_addr)
            .await
            .expect("send datagram");

        let stream = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("session accepted in time")
            .expect("session present");
        drop(stream); // the handler is done with its end of the session

        // Expect CMD_TERMINATE from the close ladder, not a 30 s idle hang.
        let mut saw_terminate = false;
        let mut received_kinds: Vec<String> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut buf = vec![0u8; 2048];
        while !saw_terminate && std::time::Instant::now() < deadline {
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(wait, client.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) => {
                    if let Some(plain) = auth::open(&Security::Simple, &[], &buf[..n]) {
                        for seg in segment::parse_all(&plain) {
                            if let Segment::Command { command, .. } = &seg {
                                if *command == segment::Command::Terminate {
                                    saw_terminate = true;
                                }
                            }
                            let label = format!("{seg:?}");
                            received_kinds.push(label.chars().take(24).collect());
                        }
                    } else {
                        received_kinds.push(format!("unparseable({n}B)"));
                    }
                }
                _ => break,
            }
        }
        assert!(
            saw_terminate,
            "dropping KcpStream must make the server send CMD_TERMINATE to the peer \
             (received datagram/symbol stream: {received_kinds:?})"
        );
    }

    // Backpressure (2026-11-26 review P2-4): when the 64-slot accept queue
    // is full, the refused session's stream is dropped — but the connection
    // stays tracked, so its pump sees read-EOF, the close ladder fires, and
    // the client gets TERMINATE instead of a silent hang.
    // multi_thread flavour for the same scheduler reason as the drop test
    // above (65 sessions with 20 ms TTI timers amplify the phenomenon).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accept_queue_backpressure_notifies_the_refused_client() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("ephemeral UDP port");
        let server_addr = socket.local_addr().unwrap();
        let config = ListenerConfig {
            security: Security::Simple,
            kcp: KcpConfig::from_inbound(0, 0, 20, 0, 0, 0),
            tti: Duration::from_millis(20),
        };
        let listener = Arc::new(MkcpListener::new(socket, config));
        let runner = Arc::clone(&listener);
        tokio::spawn(async move {
            let _ = runner.run().await;
        });

        // One client socket opening 65 sessions (distinct convs): the first
        // 64 fill the accept channel, the 65th is refused.
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("client UDP port");
        for conv in 1u16..=65 {
            let mut data = Vec::new();
            Segment::Data {
                conv,
                option: 0,
                timestamp: 0,
                number: 0,
                sending_next: 0,
                payload: b"hi".to_vec(),
            }
            .serialize(&mut data);
            let (sealed, wire) = auth::seal(&Security::Simple, &[], &data);
            client
                .send_to(&sealed[..wire], server_addr)
                .await
                .expect("send datagram");
        }
        tokio::time::sleep(Duration::from_millis(400)).await;

        // Exactly 64 sessions came through the accept channel; keep every
        // stream alive so only the refused one walks the close ladder.
        let mut streams = Vec::new();
        while let Ok(Some(stream)) =
            tokio::time::timeout(Duration::from_millis(150), listener.accept()).await
        {
            streams.push(stream);
        }
        assert_eq!(streams.len(), 64, "the accept channel holds exactly 64");

        // The refused session must have received CMD_TERMINATE (its conv,
        // whichever lost the race — loopback usually preserves order).
        let mut terminated_convs = std::collections::BTreeSet::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut buf = vec![0u8; 4096];
        while terminated_convs.is_empty() && std::time::Instant::now() < deadline {
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(wait, client.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) => {
                    if let Some(plain) = auth::open(&Security::Simple, &[], &buf[..n]) {
                        for seg in segment::parse_all(&plain) {
                            if let Segment::Command {
                                conv,
                                command: segment::Command::Terminate,
                                ..
                            } = &seg
                            {
                                terminated_convs.insert(*conv);
                            }
                        }
                    }
                }
                _ => break,
            }
        }
        assert_eq!(
            terminated_convs.len(),
            1,
            "exactly the refused session terminates, got {terminated_convs:?}"
        );
        assert!(
            terminated_convs.contains(&65) || terminated_convs.contains(&1),
            "the refused conv must be one of the opened sessions"
        );
        drop(streams); // silence the close ladders of the accepted 64
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
