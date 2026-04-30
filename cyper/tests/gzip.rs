mod server;

use std::io::Write;

use axum::extract::Request;
use flate2::{Compression, write::GzEncoder};

#[compio::test]
async fn gzip_response() {
    gzip_case(10_000, 4096).await;
}

#[compio::test]
async fn gzip_single_byte_chunks() {
    gzip_case(10, 1).await;
}

#[compio::test]
async fn test_gzip_empty_body() {
    let server = server::http(move |req: Request| async move {
        assert_eq!(req.method(), "HEAD");

        http::Response::builder()
            .header("content-encoding", "gzip")
            .body(axum::body::Body::default())
            .unwrap()
    })
    .await;

    let client = cyper::Client::new();
    let res = client
        .head(format!("http://{}/gzip", server.addr()))
        .unwrap()
        .send()
        .await
        .unwrap();

    let body = res.text().await.unwrap();
    assert_eq!(body, "");
}

#[compio::test]
async fn test_accept_header_is_not_changed_if_set() {
    let server = server::http(move |req: Request| async move {
        assert_eq!(req.headers()["accept"], "application/json");
        assert!(
            req.headers()["accept-encoding"]
                .to_str()
                .unwrap()
                .contains("gzip")
        );
        http::Response::<axum::body::Body>::default()
    })
    .await;

    let client = cyper::Client::new();

    let res = client
        .get(format!("http://{}/accept", server.addr()))
        .unwrap()
        .header("accept", "application/json")
        .unwrap()
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), http::StatusCode::OK);
}

#[compio::test]
async fn test_accept_encoding_header_is_not_changed_if_set() {
    let server = server::http(move |req: Request| async move {
        assert_eq!(req.headers()["accept"], "*/*");
        assert_eq!(req.headers()["accept-encoding"], "identity");
        http::Response::<axum::body::Body>::default()
    })
    .await;

    let client = cyper::Client::new();

    let res = client
        .get(format!("http://{}/accept-encoding", server.addr()))
        .unwrap()
        .header("accept-encoding", "identity")
        .unwrap()
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), http::StatusCode::OK);
}

async fn gzip_case(response_size: usize, chunk_size: usize) {
    let content: String = (0..response_size).map(|i| format!("test {i}")).collect();

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(content.as_bytes()).unwrap();
    let gzipped_content = encoder.finish().unwrap();

    let server = server::http(move |req: Request| {
        assert!(
            req.headers()["accept-encoding"]
                .to_str()
                .unwrap()
                .contains("gzip")
        );

        let gzipped = gzipped_content.clone();
        async move {
            let len = gzipped.len();
            let stream =
                futures_util::stream::unfold((gzipped, 0), move |(gzipped, pos)| async move {
                    let chunk = gzipped.chunks(chunk_size).nth(pos)?.to_vec();
                    Some((
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(chunk)),
                        (gzipped, pos + 1),
                    ))
                });

            let body = axum::body::Body::from_stream(stream);

            http::Response::builder()
                .header("content-encoding", "gzip")
                .header("content-length", len)
                .body(body)
                .unwrap()
        }
    })
    .await;

    let client = cyper::Client::new();

    let res = client
        .get(format!("http://{}/gzip", server.addr()))
        .unwrap()
        .send()
        .await
        .expect("response");

    let body = res.text().await.expect("text");
    assert_eq!(body, content);
}
