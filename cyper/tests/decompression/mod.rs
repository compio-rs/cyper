//! Shared helpers for compression (decompression) integration tests.

use axum::extract::Request;

/// Trait abstracting over compression formats used in tests.
pub trait Compress {
    /// The value of the `Content-Encoding` header, e.g. `"gzip"`.
    fn encoding() -> &'static str;
    /// Compress arbitrary bytes and return the compressed payload.
    fn compress(data: &[u8]) -> Vec<u8>;
}

/// Generate `n` items of `"test {i}"` concatenated together.
fn test_content(n: usize) -> String {
    (0..n).map(|i| format!("test {i}")).collect()
}

// ---------------------------------------------------------------------------
// Shared test helpers – called from per-format test files
// ---------------------------------------------------------------------------

/// Streaming-response decompression test: the server sends the
/// compressed payload in `chunk_size`-byte chunks.
pub async fn compressed_response<C: Compress>(response_size: usize, chunk_size: usize) {
    let content = test_content(response_size);
    let compressed = C::compress(content.as_bytes());
    let encoding = C::encoding();

    let server = crate::server::http(move |req: Request| {
        assert!(
            req.headers()["accept-encoding"]
                .to_str()
                .unwrap()
                .contains(encoding)
        );

        let body_data = compressed.clone();
        async move {
            let len = body_data.len();
            let stream =
                futures_util::stream::unfold((body_data, 0), move |(data, pos)| async move {
                    let chunk = data.chunks(chunk_size).nth(pos)?.to_vec();
                    Some((
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(chunk)),
                        (data, pos + 1),
                    ))
                });

            let body = axum::body::Body::from_stream(stream);

            http::Response::builder()
                .header("content-encoding", encoding)
                .header("content-length", len)
                .body(body)
                .unwrap()
        }
    })
    .await;

    let client = cyper::Client::new();

    let res = client
        .get(format!("http://{}/{encoding}", server.addr()))
        .unwrap()
        .send()
        .await
        .expect("response");

    let body = res.text().await.expect("text");
    assert_eq!(body, content);
}

/// HEAD request to a server that returns the given `Content-Encoding`
/// with an empty body – the client should decode to an empty string.
pub async fn compressed_empty_body<C: Compress>() {
    let encoding = C::encoding();

    let server = crate::server::http(move |req: Request| async move {
        assert_eq!(req.method(), "HEAD");

        http::Response::builder()
            .header("content-encoding", encoding)
            .body(axum::body::Body::default())
            .unwrap()
    })
    .await;

    let client = cyper::Client::new();
    let res = client
        .head(format!("http://{}/{encoding}", server.addr()))
        .unwrap()
        .send()
        .await
        .unwrap();

    let body = res.text().await.unwrap();
    assert_eq!(body, "");
}

/// Verify that setting a custom `accept` header does not remove the
/// auto-generated `accept-encoding` header.
pub async fn accept_header_is_not_changed_if_set<C: Compress>() {
    let encoding = C::encoding();
    let server = crate::server::http(move |req: Request| async move {
        assert_eq!(req.headers()["accept"], "application/json");
        assert!(
            req.headers()["accept-encoding"]
                .to_str()
                .unwrap()
                .contains(encoding)
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
