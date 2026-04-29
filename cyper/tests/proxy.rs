#![cfg(not(target_arch = "wasm32"))]

mod server;

use std::net::{Ipv4Addr, SocketAddr};

use axum::extract::Request;
use cyper::{
    Client,
    proxy::{NoProxy, Proxy},
};
use http::{Method, StatusCode};

// ===== HTTP Forward Proxy Tests =====

#[compio::test]
async fn http_proxy() {
    let server = server::http(move |req: Request| async move {
        assert_eq!(req.method(), Method::GET);
        // Through proxy: absolute-form URI
        assert_eq!(req.uri().to_string(), format!("http://cyper.local/prox"));
        assert_eq!(req.headers()["host"], "cyper.local");
        "OK"
    })
    .await;

    let proxy_url = format!("http://{}", server.addr());
    let client = Client::builder()
        .proxy(Proxy::http(&proxy_url).unwrap())
        .build();

    let res = client
        .get("http://cyper.local/prox")
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.text().await.unwrap(), "OK");
}

#[compio::test]
async fn http_proxy_basic_auth() {
    let server = server::http(move |req: Request| async move {
        assert_eq!(req.method(), Method::GET);
        assert_eq!(req.uri().to_string(), format!("http://cyper.local/prox"));
        assert_eq!(req.headers()["host"], "cyper.local");
        assert_eq!(
            req.headers()["proxy-authorization"],
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        "OK"
    })
    .await;

    let proxy_url = format!("http://{}", server.addr());
    let client = Client::builder()
        .proxy(
            Proxy::http(&proxy_url)
                .unwrap()
                .basic_auth("Aladdin", "open sesame"),
        )
        .build();

    let res = client
        .get("http://cyper.local/prox")
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.text().await.unwrap(), "OK");
}

#[compio::test]
async fn http_proxy_basic_auth_parsed() {
    let server = server::http(move |req: Request| async move {
        assert_eq!(req.method(), Method::GET);
        assert_eq!(req.uri().to_string(), format!("http://cyper.local/prox"));
        assert_eq!(req.headers()["host"], "cyper.local");
        assert_eq!(
            req.headers()["proxy-authorization"],
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        "OK"
    })
    .await;

    let proxy_url = format!("http://Aladdin:open sesame@{}", server.addr());
    let client = Client::builder()
        .proxy(Proxy::http(&proxy_url).unwrap())
        .build();

    let res = client
        .get("http://cyper.local/prox")
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.text().await.unwrap(), "OK");
}

#[compio::test]
async fn http_proxy_custom_auth_header() {
    let server = server::http(move |req: Request| async move {
        assert_eq!(req.method(), Method::GET);
        assert_eq!(req.uri().to_string(), format!("http://cyper.local/prox"));
        assert_eq!(req.headers()["proxy-authorization"], "testme");
        "OK"
    })
    .await;

    let proxy_url = format!("http://{}", server.addr());
    let client = Client::builder()
        .proxy(
            Proxy::http(&proxy_url)
                .unwrap()
                .custom_http_auth(http::HeaderValue::from_static("testme")),
        )
        .build();

    let res = client
        .get("http://cyper.local/prox")
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.text().await.unwrap(), "OK");
}

#[compio::test]
async fn test_no_proxy() {
    // Use the same server as both proxy and direct target.
    // With no_proxy using "*", the request should bypass the proxy
    // and go directly, resulting in origin-form URI ("/4") instead
    // of absolute-form.
    let server = server::http(move |req: Request| async move {
        assert_eq!(req.method(), Method::GET);
        // When bypassing the proxy, the URI should be origin-form (just the path)
        assert_eq!(req.uri(), "/4");
        "OK"
    })
    .await;

    let proxy_url = format!("http://{}", server.addr());
    let client = Client::builder()
        .proxy(Proxy::http(&proxy_url).unwrap().no_proxy(Some(
            NoProxy::from_string(&server.addr().ip().to_string()).unwrap(),
        )))
        .build();

    // Request to the same server; should bypass proxy and go direct
    let url = format!("http://{}/4", server.addr());
    let res = client.get(&url).unwrap().send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.text().await.unwrap(), "OK");
}

// ===== Tunnel (CONNECT) Tests =====

/// A mock proxy server that receives CONNECT requests and returns a
/// configurable response. Runs on a raw TCP listener since CONNECT is
/// not a regular HTTP request-response cycle for the server.
struct TunnelMock {
    addr: SocketAddr,
    shutdown_tx: Option<futures_channel::oneshot::Sender<()>>,
}

impl Drop for TunnelMock {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl TunnelMock {
    fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// Start a mock CONNECT proxy server. The `handler` receives the raw
/// request string (CONNECT line + headers) and should return the raw
/// HTTP response string to send back.
async fn tunnel_mock(handler: impl Fn(&str) -> String + Send + 'static) -> TunnelMock {
    use compio::{
        BufResult,
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    let listener = TcpListener::bind(&(Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = futures_channel::oneshot::channel();

    compio::runtime::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        let mut buf = Vec::<u8>::with_capacity(4096);
        loop {
            let BufResult(res, b) = stream.append(buf).await;
            let n = res.unwrap();
            buf = b;
            if buf.len() >= 4 && buf[buf.len() - 4..] == *b"\r\n\r\n" {
                break;
            }
            if n == 0 {
                return;
            }
        }

        let request = std::str::from_utf8(&buf).unwrap();
        let response = handler(request);

        let BufResult(res, _) = stream.write_all(response).await;
        res.unwrap();

        // Keep the connection alive until shutdown
        let _ = shutdown_rx.await;
    })
    .detach();

    TunnelMock {
        addr,
        shutdown_tx: Some(shutdown_tx),
    }
}

#[cfg(tls)]
#[compio::test]
async fn tunnel_detects_unsuccessful() {
    let mock = tunnel_mock(move |request: &str| {
        assert!(
            request.starts_with("CONNECT"),
            "Expected CONNECT, got: {request}"
        );
        assert!(
            request.contains("cyper.local:443"),
            "Expected target host:port in CONNECT, got: {request}"
        );
        "HTTP/1.1 400 Bad Request\r\n\r\n".to_string()
    })
    .await;

    let proxy_url = format!("http://{}", mock.addr());
    let client = Client::builder()
        .proxy(Proxy::https(&proxy_url).unwrap())
        .danger_accept_invalid_certs(true)
        .build();

    let err = client
        .get("https://cyper.local/prox")
        .unwrap()
        .send()
        .await
        .unwrap_err();

    let err_msg = format!("{err:?}");
    assert!(
        err_msg.contains("400")
            || err_msg.contains("unsuccessful")
            || err_msg.contains("Unsuccessful")
            || err_msg.contains("Bad Request"),
        "expected error to mention 400, got: {err_msg}"
    );
}

#[cfg(tls)]
#[compio::test]
async fn tunnel_includes_proxy_auth() {
    let mock = tunnel_mock(move |request: &str| {
        assert!(request.starts_with("CONNECT"));
        assert!(
            request.contains("Proxy-Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="),
            "Expected proxy-authorization header, got: {request}"
        );
        "HTTP/1.1 400 Bad Request\r\n\r\n".to_string()
    })
    .await;

    let proxy_url = format!("http://Aladdin:open sesame@{}", mock.addr());
    let client = Client::builder()
        .proxy(Proxy::https(&proxy_url).unwrap())
        .danger_accept_invalid_certs(true)
        .build();

    let err = client
        .get("https://cyper.local/prox")
        .unwrap()
        .send()
        .await
        .unwrap_err();

    let err_msg = format!("{err:?}");
    assert!(
        err_msg.contains("400")
            || err_msg.contains("unsuccessful")
            || err_msg.contains("Unsuccessful")
            || err_msg.contains("Bad Request"),
        "expected error from failed CONNECT, got: {err_msg}"
    );
}

// ===== Multiple Proxies =====

#[compio::test]
async fn proxy_https_matches_https_only() {
    let server = server::http(move |_req: Request| async move {
        // This handler should not be reached since https-only proxy
        // does not match http:// requests
        "SHOULD_NOT_REACH"
    })
    .await;

    let proxy_url = format!("http://{}", server.addr());
    let client = Client::builder()
        .proxy(Proxy::https(&proxy_url).unwrap())
        .build();

    // An HTTP request should NOT go through the HTTPS proxy
    let res = client.get("http://cyper.local/prox").unwrap().send().await;

    // With an https-only proxy and an http request, the request goes direct.
    // "cyper.local" won't resolve, so we'll get a connection error.
    assert!(res.is_err());
}

#[compio::test]
async fn proxy_multiple_matches_correct() {
    let server = server::http(move |req: Request| async move {
        assert_eq!(req.method(), Method::GET);
        assert_eq!(req.uri().to_string(), format!("http://cyper.local/prox"));
        "OK"
    })
    .await;

    let proxy_url = format!("http://{}", server.addr());
    // Add an HTTPS proxy first, then an HTTP proxy – the HTTP matcher
    // should be selected for http:// URLs
    let client = Client::builder()
        .proxy(Proxy::https("http://unreachable.example").unwrap())
        .proxy(Proxy::http(&proxy_url).unwrap())
        .build();

    let res = client
        .get("http://cyper.local/prox")
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.text().await.unwrap(), "OK");
}
