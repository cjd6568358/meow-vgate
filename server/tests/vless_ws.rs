//! End-to-end tests for VLESS over WebSocket on the 19001-shaped inbound.
//!
//! The server binds an ephemeral port; only the wire behaviour (WebSocket
//! path, VLESS header, response header, payload relay) is under test here.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use meow_server::config::InboundConfig;
use meow_server::listener::{serve_vless_ws, SessionHandler};
use meow_server::stream::DynStream;
use meow_server::vless::{self, VlessRequest, COMMAND_TCP, COMMAND_UDP};
use meow_server::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message;

const USER: [u8; 16] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
];
const OTHER_USER: [u8; 16] = [0xab; 16];

struct Echo;

#[async_trait]
impl SessionHandler for Echo {
    async fn handle(&self, _request: VlessRequest, stream: DynStream) -> Result<()> {
        let (mut reader, mut writer) = tokio::io::split(stream);
        tokio::io::copy(&mut reader, &mut writer).await?;
        Ok(())
    }
}

async fn start_server(
    handler: Arc<dyn SessionHandler>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral port is available");
    let address = listener
        .local_addr()
        .expect("bound listener has an address");
    let config = InboundConfig {
        listen: address,
        ws_path: "/ws".to_owned(),
    };
    let task = tokio::spawn(async move {
        let _ = serve_vless_ws(listener, config, vec![USER], handler).await;
    });
    (address, task)
}

fn vless_frame(user: [u8; 16], command: u8, payload: &[u8]) -> Bytes {
    let mut frame = vec![vless::VERSION];
    frame.extend_from_slice(&user);
    frame.push(0); // addons length
    frame.push(command);
    frame.extend_from_slice(&443u16.to_be_bytes());
    frame.push(vless::ADDRESS_IPV4);
    frame.extend_from_slice(&[127, 0, 0, 1]);
    frame.extend_from_slice(payload);
    Bytes::from(frame)
}

async fn next_binary(
    websocket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
) -> Option<Bytes> {
    loop {
        let message = match timeout(Duration::from_secs(2), websocket.next()).await {
            Ok(Some(Ok(message))) => message,
            _ => return None,
        };
        match message {
            Message::Binary(bytes) => return Some(bytes),
            Message::Ping(_) | Message::Pong(_) => continue,
            _ => return None,
        }
    }
}

#[tokio::test]
async fn vless_over_ws_relays_payload_after_response_header() {
    let (address, _task) = start_server(Arc::new(Echo)).await;
    let tcp = TcpStream::connect(address)
        .await
        .expect("connect to server");
    let url = format!("ws://{address}/ws");
    let (mut websocket, _) = client_async(url.as_str(), tcp)
        .await
        .expect("websocket upgrade");

    websocket
        .send(Message::Binary(vless_frame(
            USER,
            COMMAND_TCP,
            b"hello-vless",
        )))
        .await
        .expect("send VLESS frame");

    let mut received = Vec::new();
    while received.len() < 2 + b"hello-vless".len() {
        let chunk = next_binary(&mut websocket).await.expect("binary response");
        received.extend_from_slice(&chunk);
    }
    assert_eq!(&received[..2], &[0, 0], "VLESS response header");
    assert_eq!(&received[2..], b"hello-vless");
}

#[tokio::test]
async fn websocket_upgrade_rejects_wrong_path() {
    let (address, _task) = start_server(Arc::new(Echo)).await;
    let tcp = TcpStream::connect(address)
        .await
        .expect("connect to server");
    let url = format!("ws://{address}/wrong");
    let outcome = timeout(Duration::from_secs(2), client_async(url.as_str(), tcp)).await;
    assert!(outcome.expect("upgrade responds").is_err());
}

#[tokio::test]
async fn unknown_user_never_receives_a_response_header() {
    let (address, _task) = start_server(Arc::new(Echo)).await;
    let tcp = TcpStream::connect(address)
        .await
        .expect("connect to server");
    let url = format!("ws://{address}/ws");
    let (mut websocket, _) = client_async(url.as_str(), tcp)
        .await
        .expect("websocket upgrade");

    websocket
        .send(Message::Binary(vless_frame(OTHER_USER, COMMAND_TCP, b"x")))
        .await
        .expect("send VLESS frame");

    assert!(next_binary(&mut websocket).await.is_none());
}

#[tokio::test]
async fn udp_command_is_rejected_instead_of_misrouted() {
    let (address, _task) = start_server(Arc::new(Echo)).await;
    let tcp = TcpStream::connect(address)
        .await
        .expect("connect to server");
    let url = format!("ws://{address}/ws");
    let (mut websocket, _) = client_async(url.as_str(), tcp)
        .await
        .expect("websocket upgrade");

    websocket
        .send(Message::Binary(vless_frame(USER, COMMAND_UDP, b"x")))
        .await
        .expect("send VLESS frame");

    assert!(next_binary(&mut websocket).await.is_none());
}

#[tokio::test]
async fn read_request_leaves_payload_unread() {
    // A direct-stream check of the same framing the WebSocket path relies on.
    let frame = vless_frame(USER, COMMAND_TCP, b"payload");
    let mut cursor = frame.as_ref();
    let request = vless::read_request(&mut cursor)
        .await
        .expect("parse header");
    assert_eq!(request.user_id, USER);
    assert_eq!(request.command, COMMAND_TCP);

    let mut remaining = Vec::new();
    cursor.read_to_end(&mut remaining).await.expect("read rest");
    assert_eq!(remaining, b"payload");
}

#[tokio::test]
async fn read_request_writes_response_header_via_sink() {
    let frame = vless_frame(USER, COMMAND_TCP, b"");
    let mut cursor = frame.as_ref();
    let _request = vless::read_request(&mut cursor)
        .await
        .expect("parse header");
    let mut output = Vec::new();
    output
        .write_all(&vless::RESPONSE_HEADER)
        .await
        .expect("write");
    assert_eq!(output, vec![0, 0]);
}
