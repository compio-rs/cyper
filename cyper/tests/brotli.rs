mod server;

use std::io::Read;

use axum::extract::Request;

#[compio::test]
async fn brotli_response() {
    brotli_case(10_000, 4096).await;
}

#[compio::test]
async fn brotli_single_byte_chunks() {
    brotli_case(10, 1).await;
}

#[compio::test]
async fn test_brotli_empty_body() {
    let server = server::http(move |req: Request| async move {
        assert_eq!(req.method(), "HEAD");

        http::Response::builder()
            .header("content-encoding", "br")
            .body(axum::body::Body::default())
            .unwrap()
    })
    .await;

    let client = cyper::Client::new();
    let res = client
        .head(format!("http://{}/brotli", server.addr()))
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
                .contains("br")
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

async fn brotli_case(response_size: usize, chunk_size: usize) {
    let content: String = (0..response_size).map(|i| format!("test {i}")).collect();

    let mut encoder = brotli_crate::CompressorReader::new(content.as_bytes(), 4096, 5, 20);
    let mut brotlied_content = Vec::new();
    encoder.read_to_end(&mut brotlied_content).unwrap();

    let server = server::http(move |req: Request| {
        assert!(
            req.headers()["accept-encoding"]
                .to_str()
                .unwrap()
                .contains("br")
        );

        let brotlied = brotlied_content.clone();
        async move {
            let len = brotlied.len();
            let stream =
                futures_util::stream::unfold((brotlied, 0), move |(brotlied, pos)| async move {
                    let chunk = brotlied.chunks(chunk_size).nth(pos)?.to_vec();
                    Some((
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(chunk)),
                        (brotlied, pos + 1),
                    ))
                });

            let body = axum::body::Body::from_stream(stream);

            http::Response::builder()
                .header("content-encoding", "br")
                .header("content-length", len)
                .body(body)
                .unwrap()
        }
    })
    .await;

    let client = cyper::Client::new();

    let res = client
        .get(format!("http://{}/brotli", server.addr()))
        .unwrap()
        .send()
        .await
        .expect("response");

    let body = res.text().await.expect("text");
    assert_eq!(body, content);
}
