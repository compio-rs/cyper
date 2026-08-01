//! WebSocket round-trip latency distribution for Cyper and Axum servers,
//! measured with Tokio and Compio clients.
//!
//! Every round trip is timed individually and reported as percentiles. A mean
//! hides exactly the stalls that matter, and a benchmark harness that averages
//! thousands of round trips into one sample cannot show a tail at all.
//!
//! Each cell holds a fixed number of connections with one request in flight
//! each, which lets a completion-based runtime overlap submissions. The loop is
//! closed: a connection sends again only after its previous reply arrives, so
//! these numbers are service time at a given queue depth, not response time at
//! a target rate. An open-loop mode would be needed for the latter.
//!
//! Client and server share one host over loopback and each run on a single
//! thread, so client cost is inside every sample. Compare servers against each
//! other under an identical client rather than reading absolute values.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::mpsc::sync_channel,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use axum::{Router, response::Response, routing::any};
use compio::{
    net::{TcpListener, TcpStream},
    runtime::Runtime,
    ws::{
        WebSocketStream as CompioWebSocketStream,
        tungstenite::{Bytes as CompioBytes, Message as CompioMessage},
    },
};
use cyper_axum::ws::{
    Message as CyperMessage, WebSocket as CyperWebSocket, WebSocketUpgrade as CyperWebSocketUpgrade,
};
use futures_channel::oneshot;
use futures_util::{SinkExt, StreamExt, future::join_all};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream as TokioWebSocketStream, connect_async,
    tungstenite::{Bytes as TokioBytes, Message as TokioMessage},
};

const KIB: usize = 1024;
const MIB: usize = KIB * KIB;

/// Payload size paired with the number of connections kept in flight.
const CELLS: &[(usize, usize)] = &[
    (64, 1),
    (64, 16),
    (64, 64),
    (16 * KIB, 1),
    (16 * KIB, 16),
    (16 * KIB, 64),
    (MIB, 1),
    (MIB, 8),
];

/// Round trips to measure per cell, summed over all connections.
fn planned_round_trips(payload_size: usize) -> usize {
    if payload_size <= KIB {
        20_000
    } else if payload_size <= 64 * KIB {
        8_000
    } else {
        1_000
    }
}

type TokioWebSocket = TokioWebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

// ===== Reporting =====

struct Report {
    /// Nanoseconds per round trip, sorted ascending.
    latencies: Vec<u64>,
    wall: Duration,
    bytes: u64,
}

impl Report {
    fn new(mut latencies: Vec<u64>, wall: Duration, bytes: u64) -> Self {
        latencies.sort_unstable();
        Self {
            latencies,
            wall,
            bytes,
        }
    }

    /// Nearest-rank percentile, so the reported value is always an observed
    /// sample rather than an interpolation between two of them.
    fn percentile(&self, quantile: f64) -> u64 {
        let rank = (quantile * self.latencies.len() as f64).ceil() as usize;
        self.latencies[rank.clamp(1, self.latencies.len()) - 1]
    }

    fn max(&self) -> u64 {
        *self.latencies.last().expect("no samples recorded")
    }

    fn throughput_mib(&self) -> f64 {
        self.bytes as f64 / self.wall.as_secs_f64() / MIB as f64
    }
}

fn format_nanos(nanos: u64) -> String {
    if nanos < 1_000 {
        format!("{nanos} ns")
    } else if nanos < 1_000_000 {
        format!("{:.2} µs", nanos as f64 / 1e3)
    } else {
        format!("{:.2} ms", nanos as f64 / 1e6)
    }
}

fn format_bytes(bytes: usize) -> String {
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{} MiB", bytes / MIB)
    }
}

fn print_row(client: &str, server: &str, report: &Report) {
    println!(
        "  {client:<7}{server:<8}{:>10}{:>10}{:>11}{:>11}{:>13.1}",
        format_nanos(report.percentile(0.5)),
        format_nanos(report.percentile(0.99)),
        format_nanos(report.percentile(0.999)),
        format_nanos(report.max()),
        report.throughput_mib(),
    );
}

// ===== Servers =====

struct RunningServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("WebSocket server thread panicked");
        }
    }
}

fn spawn_cyper_server() -> RunningServer {
    let (addr_tx, addr_rx) = sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let thread = thread::spawn(move || {
        Runtime::new().unwrap().block_on(async move {
            let app = Router::new().route("/ws", any(cyper_echo_handler));
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            addr_tx.send(listener.local_addr().unwrap()).unwrap();

            cyper_axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
    });

    RunningServer {
        addr: addr_rx.recv().unwrap(),
        shutdown: Some(shutdown_tx),
        thread: Some(thread),
    }
}

fn spawn_axum_server() -> RunningServer {
    let (addr_tx, addr_rx) = sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let thread = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let app = Router::new().route("/ws", any(axum_echo_handler));
                let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                    .await
                    .unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();

                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .unwrap();
            });
    });

    RunningServer {
        addr: addr_rx.recv().unwrap(),
        shutdown: Some(shutdown_tx),
        thread: Some(thread),
    }
}

async fn cyper_echo_handler(ws: CyperWebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket: CyperWebSocket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            if matches!(&message, CyperMessage::Text(_) | CyperMessage::Binary(_)) {
                if socket.send(message).await.is_err() {
                    break;
                }
            } else if matches!(message, CyperMessage::Close(_)) {
                break;
            }
        }
    })
}

async fn axum_echo_handler(ws: axum::extract::ws::WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            if matches!(
                &message,
                axum::extract::ws::Message::Text(_) | axum::extract::ws::Message::Binary(_)
            ) {
                if socket.send(message).await.is_err() {
                    break;
                }
            } else if matches!(message, axum::extract::ws::Message::Close(_)) {
                break;
            }
        }
    })
}

// ===== Clients =====

/// Echoes `message` back and forth `count` times, timing each round trip.
///
/// The reply is reused as the next request so that no allocation lands inside
/// the timed region.
async fn tokio_round_trips(
    ws: &mut TokioWebSocket,
    message: &mut TokioMessage,
    count: usize,
    latencies: &mut Vec<u64>,
) {
    for _ in 0..count {
        let request = std::mem::replace(message, TokioMessage::Binary(TokioBytes::new()));
        let at = Instant::now();
        ws.send(request).await.unwrap();
        *message = ws.next().await.unwrap().unwrap();
        latencies.push(at.elapsed().as_nanos() as u64);
    }
}

async fn compio_round_trips(
    ws: &mut CompioWebSocketStream<TcpStream>,
    message: &mut CompioMessage,
    count: usize,
    latencies: &mut Vec<u64>,
) {
    for _ in 0..count {
        let request = std::mem::replace(message, CompioMessage::Binary(CompioBytes::new()));
        let at = Instant::now();
        ws.send(request).await.unwrap();
        *message = ws.read().await.unwrap();
        latencies.push(at.elapsed().as_nanos() as u64);
    }
}

/// Warms up every connection, then measures a second pass over the same
/// connections so the reported wall clock covers the measured phase only.
macro_rules! measure {
    (
        $addr:expr,
        $payload_size:expr,
        $concurrency:expr,
        $per_connection:expr,connect =
        $connect:expr,message =
        $message:expr,round_trips =
        $round_trips:expr,
    ) => {{
        let warmup = ($per_connection / 10).clamp(1, 200);

        let mut connections = Vec::with_capacity($concurrency);
        let mut messages = Vec::with_capacity($concurrency);
        let mut scratch = Vec::with_capacity($concurrency);
        let mut measured = Vec::with_capacity($concurrency);
        for _ in 0..$concurrency {
            connections.push($connect($addr).await);
            messages.push($message(vec![0x42; $payload_size]));
            scratch.push(Vec::with_capacity(warmup));
            measured.push(Vec::with_capacity($per_connection));
        }

        join_all(
            connections
                .iter_mut()
                .zip(messages.iter_mut())
                .zip(scratch.iter_mut())
                .map(|((ws, message), latencies)| $round_trips(ws, message, warmup, latencies)),
        )
        .await;

        let started = Instant::now();
        join_all(
            connections
                .iter_mut()
                .zip(messages.iter_mut())
                .zip(measured.iter_mut())
                .map(|((ws, message), latencies)| {
                    $round_trips(ws, message, $per_connection, latencies)
                }),
        )
        .await;
        let wall = started.elapsed();

        for mut ws in connections {
            ws.close(None).await.unwrap();
        }

        let latencies: Vec<u64> = measured.concat();
        let bytes = 2 * $payload_size as u64 * latencies.len() as u64;
        Report::new(latencies, wall, bytes)
    }};
}

fn run_tokio(
    addr: SocketAddr,
    payload_size: usize,
    concurrency: usize,
    per_connection: usize,
) -> Report {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            measure!(
                addr,
                payload_size,
                concurrency,
                per_connection,
                connect = async |addr: SocketAddr| {
                    connect_async(format!("ws://{addr}/ws"))
                        .await
                        .expect("WebSocket handshake failed")
                        .0
                },
                message = |data: Vec<u8>| TokioMessage::Binary(data.into()),
                round_trips = tokio_round_trips,
            )
        })
}

fn run_compio(
    addr: SocketAddr,
    payload_size: usize,
    concurrency: usize,
    per_connection: usize,
) -> Report {
    Runtime::new().unwrap().block_on(async move {
        measure!(
            addr,
            payload_size,
            concurrency,
            per_connection,
            connect = async |addr: SocketAddr| {
                let stream = TcpStream::connect(addr).await.unwrap();
                compio::ws::client_async(format!("ws://{addr}/ws"), stream)
                    .await
                    .expect("WebSocket handshake failed")
                    .0
            },
            message = |data: Vec<u8>| CompioMessage::Binary(data.into()),
            round_trips = compio_round_trips,
        )
    })
}

fn main() {
    let cyper = spawn_cyper_server();
    let axum = spawn_axum_server();

    for &(payload_size, concurrency) in CELLS {
        let per_connection = planned_round_trips(payload_size)
            .div_ceil(concurrency)
            .max(1);
        println!(
            "\npayload {}, {concurrency} connection(s), {} round trips each",
            format_bytes(payload_size),
            per_connection,
        );
        println!(
            "  {:<7}{:<8}{:>10}{:>10}{:>11}{:>11}{:>13}",
            "client", "server", "p50", "p99", "p99.9", "max", "MiB/s"
        );

        for (client, run) in [
            (
                "tokio",
                run_tokio as fn(SocketAddr, usize, usize, usize) -> Report,
            ),
            ("compio", run_compio),
        ] {
            print_row(
                client,
                "cyper",
                &run(cyper.addr, payload_size, concurrency, per_connection),
            );
            print_row(
                client,
                "axum",
                &run(axum.addr, payload_size, concurrency, per_connection),
            );
        }
    }
}
