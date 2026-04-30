mod server;

use axum::extract::Request;

#[compio::test]
async fn zstd_response() {
    zstd_case(10_000, 4096).await;
}

#[compio::test]
async fn zstd_single_byte_chunks() {
    zstd_case(10, 1).await;
}

#[compio::test]
async fn test_zstd_empty_body() {
    let server = server::http(move |req: Request| async move {
        assert_eq!(req.method(), "HEAD");

        http::Response::builder()
            .header("content-encoding", "zstd")
            .body(axum::body::Body::default())
            .unwrap()
    })
    .await;

    let client = cyper::Client::new();
    let res = client
        .head(format!("http://{}/zstd", server.addr()))
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
                .contains("zstd")
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

async fn zstd_case(response_size: usize, chunk_size: usize) {
    let content: String = (0..response_size).map(|i| format!("test {i}")).collect();

    let zstded_content = zstd_crate::encode_all(content.as_bytes(), 3).unwrap();

    let server = server::http(move |req: Request| {
        assert!(
            req.headers()["accept-encoding"]
                .to_str()
                .unwrap()
                .contains("zstd")
        );

        let zstded = zstded_content.clone();
        async move {
            let len = zstded.len();
            let stream =
                futures_util::stream::unfold((zstded, 0), move |(zstded, pos)| async move {
                    let chunk = zstded.chunks(chunk_size).nth(pos)?.to_vec();
                    Some((
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(chunk)),
                        (zstded, pos + 1),
                    ))
                });

            let body = axum::body::Body::from_stream(stream);

            http::Response::builder()
                .header("content-encoding", "zstd")
                .header("content-length", len)
                .body(body)
                .unwrap()
        }
    })
    .await;

    let client = cyper::Client::new();

    let res = client
        .get(format!("http://{}/zstd", server.addr()))
        .unwrap()
        .send()
        .await
        .expect("response");

    let body = res.text().await.expect("text");
    assert_eq!(body, content);
}
