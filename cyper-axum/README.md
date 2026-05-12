# cyper-axum

An `axum` adapter runs on `compio`.

## Features

* `ws`: WebSocket support

## Usage

```
cargo add cyper-axum
```
For WebSocket support:
```
cargo add cyper-axum --features ws
```

Example:
```rust
use axum::{Router, routing::get};

let router = Router::new().route("/", get(|| async { "Hello, World!" }));

let listener = compio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
cyper_axum::serve(listener, router).await.unwrap();
```
