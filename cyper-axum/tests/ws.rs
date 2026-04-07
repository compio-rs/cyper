use std::net::Ipv4Addr;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::Response,
    routing::any,
    Router,
};
use compio::net::TcpListener;
use futures_channel::oneshot;

async fn ws_handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Text(text) => {
                if socket.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

#[compio::test]
async fn websocket_echo() {
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let app = Router::new().route("/ws", any(ws_handler));

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

    // Connect with compio-ws client
    let stream = compio::net::TcpStream::connect(addr).await.unwrap();
    let (mut ws, _resp) =
        compio_ws::client_async(format!("ws://{addr}/ws"), stream)
            .await
            .expect("WebSocket handshake failed");

    // Send a message
    ws.send(compio_ws::tungstenite::Message::Text("hello".into()))
        .await
        .expect("failed to send message");

    // Receive the echo
    let msg = ws.read().await.expect("failed to read message");
    assert_eq!(msg, compio_ws::tungstenite::Message::Text("hello".into()));

    // Clean up
    ws.close(None).await.ok();
    drop(shutdown_tx);
}
