use std::net::{Ipv4Addr, SocketAddr};

use axum::{Router, response::Response, routing::any};
use compio::{net::TcpListener, ws::tungstenite::Message as TsMessage};
use cyper_axum::ws::{Message, WebSocket, WebSocketUpgrade};
use futures_channel::oneshot;

// ===== Helpers =====

/// Spawn a cyper-axum server with the given router, return its address and a
/// shutdown handle (drop to stop).
async fn spawn_server(app: Router) -> (SocketAddr, oneshot::Sender<()>) {
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0u16))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();

    compio::runtime::spawn(async move {
        cyper_axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
    })
    .detach();

    (addr, shutdown_tx)
}

/// Connect a compio-ws client to the given address at `/ws`.
async fn connect(addr: SocketAddr) -> compio::ws::WebSocketStream<compio::net::TcpStream> {
    let stream = compio::net::TcpStream::connect(addr).await.unwrap();
    let (ws, _resp) = compio::ws::client_async(format!("ws://{addr}/ws"), stream)
        .await
        .expect("WebSocket handshake failed");
    ws
}

// ===== Echo handler (echoes text and binary) =====

async fn echo_handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(echo_socket)
}

async fn echo_socket(mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Text(_) | Message::Binary(_) if socket.send(msg.clone()).await.is_err() => {
                break;
            }

            Message::Close(_) => break,
            _ => {}
        }
    }
}

// ===== Tests =====

#[compio::test]
async fn echo_text() {
    let app = Router::new().route("/ws", any(echo_handler));
    let (addr, _shutdown) = spawn_server(app).await;
    let mut ws = connect(addr).await;

    ws.send(TsMessage::Text("hello".into())).await.unwrap();
    let msg = ws.read().await.unwrap();
    assert_eq!(msg, TsMessage::Text("hello".into()));

    ws.close(None).await.ok();
}

#[compio::test]
async fn echo_binary() {
    let app = Router::new().route("/ws", any(echo_handler));
    let (addr, _shutdown) = spawn_server(app).await;
    let mut ws = connect(addr).await;

    let data = compio::ws::tungstenite::Bytes::from_static(b"\x00\x01\x02\x03");
    ws.send(TsMessage::Binary(data.clone())).await.unwrap();
    let msg = ws.read().await.unwrap();
    assert_eq!(msg, TsMessage::Binary(data));

    ws.close(None).await.ok();
}

#[compio::test]
async fn multiple_messages() {
    let app = Router::new().route("/ws", any(echo_handler));
    let (addr, _shutdown) = spawn_server(app).await;
    let mut ws = connect(addr).await;

    for i in 0..10 {
        let text = format!("message {i}");
        ws.send(TsMessage::Text(text.clone().into())).await.unwrap();
        let msg = ws.read().await.unwrap();
        assert_eq!(msg, TsMessage::Text(text.into()));
    }

    ws.close(None).await.ok();
}

#[compio::test]
async fn large_message() {
    let app = Router::new().route("/ws", any(echo_handler));
    let (addr, _shutdown) = spawn_server(app).await;
    let mut ws = connect(addr).await;

    // 256 KiB text message — exercises buffer growth in UpgradedIo
    let big = "x".repeat(256 * 1024);
    ws.send(TsMessage::Text(big.clone().into())).await.unwrap();
    let msg = ws.read().await.unwrap();
    assert_eq!(msg, TsMessage::Text(big.into()));

    ws.close(None).await.ok();
}

#[compio::test]
async fn ping_pong() {
    // tungstenite auto-replies to pings, so we send a ping and expect a pong
    let app = Router::new().route("/ws", any(echo_handler));
    let (addr, _shutdown) = spawn_server(app).await;
    let mut ws = connect(addr).await;

    ws.send(TsMessage::Ping(
        compio::ws::tungstenite::Bytes::from_static(b"ping"),
    ))
    .await
    .unwrap();
    let msg = ws.read().await.unwrap();
    assert_eq!(
        msg,
        TsMessage::Pong(compio::ws::tungstenite::Bytes::from_static(b"ping"))
    );

    ws.close(None).await.ok();
}

#[compio::test]
async fn graceful_close() {
    let app = Router::new().route("/ws", any(echo_handler));
    let (addr, _shutdown) = spawn_server(app).await;
    let mut ws = connect(addr).await;

    // Send a text, get echo, then close with a reason
    ws.send(TsMessage::Text("before close".into()))
        .await
        .unwrap();
    let msg = ws.read().await.unwrap();
    assert_eq!(msg, TsMessage::Text("before close".into()));

    ws.close(Some(compio::ws::tungstenite::protocol::CloseFrame {
        code: compio::ws::tungstenite::protocol::frame::coding::CloseCode::Normal,
        reason: "done".into(),
    }))
    .await
    .unwrap();
}

#[compio::test]
async fn protocol_negotiation() {
    async fn handler(ws: WebSocketUpgrade) -> Response {
        let ws = ws.protocols(["graphql-ws", "echo"]);
        assert_eq!(ws.selected_protocol().unwrap(), "echo");
        ws.on_upgrade(|mut socket| async move {
            assert_eq!(socket.protocol().unwrap(), "echo");
            while let Some(Ok(msg)) = socket.recv().await {
                match msg {
                    Message::Text(_) | Message::Binary(_) => {
                        if socket.send(msg).await.is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        })
    }

    let app = Router::new().route("/ws", any(handler));
    let (addr, _shutdown) = spawn_server(app).await;

    let stream = compio::net::TcpStream::connect(addr).await.unwrap();
    let req = compio::ws::tungstenite::client::IntoClientRequest::into_client_request(format!(
        "ws://{addr}/ws"
    ))
    .unwrap();
    // Build request with subprotocol header
    let mut req = req;
    req.headers_mut()
        .insert("Sec-WebSocket-Protocol", "echo, other".parse().unwrap());
    let (mut ws, resp) = compio::ws::client_async(req, stream).await.unwrap();

    assert_eq!(
        resp.headers().get("Sec-WebSocket-Protocol").unwrap(),
        "echo"
    );

    ws.send(TsMessage::Text("proto test".into())).await.unwrap();
    let msg = ws.read().await.unwrap();
    assert_eq!(msg, TsMessage::Text("proto test".into()));

    ws.close(None).await.ok();
}

#[compio::test]
async fn server_close() {
    // Server sends close after first message
    async fn handler(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(|mut socket| async move {
            if let Some(Ok(_msg)) = socket.recv().await {
                socket
                    .close(Some(cyper_axum::ws::CloseFrame {
                        code: cyper_axum::ws::CloseCode::Normal,
                        reason: "bye".into(),
                    }))
                    .await
                    .ok();
            }
        })
    }

    let app = Router::new().route("/ws", any(handler));
    let (addr, _shutdown) = spawn_server(app).await;
    let mut ws = connect(addr).await;

    ws.send(TsMessage::Text("trigger close".into()))
        .await
        .unwrap();

    // Should receive the close frame
    let msg = ws.read().await.unwrap();
    match msg {
        TsMessage::Close(Some(frame)) => {
            assert_eq!(
                frame.code,
                compio::ws::tungstenite::protocol::frame::coding::CloseCode::Normal
            );
            assert_eq!(frame.reason.as_str(), "bye");
        }
        TsMessage::Close(None) => {} // also acceptable
        other => panic!("expected Close, got {other:?}"),
    }

    ws.close(None).await.ok();
}

#[compio::test]
async fn client_drop_does_not_panic_server() {
    // Ensure the server doesn't panic when the client disconnects abruptly
    let app = Router::new().route("/ws", any(echo_handler));
    let (addr, _shutdown) = spawn_server(app).await;
    let mut ws = connect(addr).await;

    ws.send(TsMessage::Text("hello".into())).await.unwrap();
    let _ = ws.read().await.unwrap();

    // Drop without close — server should handle gracefully
    drop(ws);

    // Verify server is still alive by connecting again
    let mut ws2 = connect(addr).await;
    ws2.send(TsMessage::Text("still alive".into()))
        .await
        .unwrap();
    let msg = ws2.read().await.unwrap();
    assert_eq!(msg, TsMessage::Text("still alive".into()));

    ws2.close(None).await.ok();
}
