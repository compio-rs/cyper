use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::net::{Ipv4Addr, SocketAddr};

use axum::{Router, response::Response, routing::any};
use compio::{
    net::{TcpListener, TcpStream},
    runtime::Runtime,
    ws::{WebSocketStream, tungstenite::Message as TsMessage},
};
use cyper_axum::ws::{Message, WebSocket, WebSocketUpgrade};

async fn spawn_server(app: Router) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();

    compio::runtime::spawn(async move {
        cyper_axum::serve(listener, app).await.unwrap();
    })
    .detach();

    addr
}

async fn connect(addr: SocketAddr) -> WebSocketStream<TcpStream> {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (ws, _) = compio::ws::client_async(format!("ws://{addr}/ws"), stream)
        .await
        .expect("WebSocket handshake failed");
    ws
}

async fn echo_handler(ws: WebSocketUpgrade) -> Response {
    let closure = async |mut socket: WebSocket| {
        while let Some(Ok(msg)) = socket.recv().await {
            match msg {
                Message::Text(_) if socket.send(msg).await.is_err() => {
                    break;
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };

    ws.on_upgrade(closure)
}

fn measure(c: &mut Criterion) {
    #[allow(non_upper_case_globals)]
    const KiB: usize = 1024;

    let mut group = c.benchmark_group("Perf");
    for size in [128 * KiB, 256 * KiB, 512 * KiB].iter() {
        group.throughput(Throughput::Bytes((*size * 2) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            let runtime = Runtime::new().unwrap();
            b.iter_custom(|iters| {
                runtime.block_on(async move {
                    let app = Router::new().route("/ws", any(echo_handler));
                    let addr = spawn_server(app).await;
                    let mut ws = connect(addr).await;
                    let data = "x".repeat(size);
                    let mut message = TsMessage::Text(data.into());
                    let start = std::time::Instant::now();
                    for _ in 0..iters {
                        std::hint::black_box(ws.send(message).await).unwrap();
                        message = std::hint::black_box(ws.read().await.unwrap());
                    }
                    ws.close(None).await.unwrap();
                    start.elapsed()
                })
            });
        });
    }
    group.finish();
}

criterion_group!(benches, measure);
criterion_main!(benches);
