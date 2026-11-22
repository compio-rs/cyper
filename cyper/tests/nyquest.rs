use std::sync::Once;

#[cfg(feature = "nyquest-async")]
use nyquest::client::CustomProxy;
use nyquest::{ClientBuilder, Request};

mod server;

static REGISTER: Once = Once::new();

fn register() {
    REGISTER.call_once(|| {
        cyper::nyquest::register();
    })
}

#[test]
fn register_backend() {
    register();
    register(); // should not panic
}

fn builder() -> ClientBuilder {
    register();
    ClientBuilder::default()
}

#[cfg(feature = "nyquest-async")]
#[compio::test]
async fn response_text_async() {
    let server = server::http(move |_req| async { "Hello" }).await;

    let client = builder().build_async().await.unwrap();

    let text = client
        .request(Request::get(format!("http://{}/text", server.addr())))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!("Hello", text);
}

#[cfg(feature = "nyquest-blocking")]
#[compio::test]
async fn response_text_blocking() {
    use futures_channel::oneshot;

    let server = server::http(move |_req| async { "Hello" }).await;

    let (tx, rx) = oneshot::channel();

    let handle = std::thread::spawn(move || {
        let client = builder().build_blocking().unwrap();

        let text = client
            .request(Request::get(format!("http://{}/text", server.addr())))
            .unwrap()
            .text()
            .unwrap();
        assert_eq!("Hello", text);
        tx.send(()).unwrap();
    });

    rx.await.unwrap();

    if let Err(e) = handle.join() {
        std::panic::resume_unwind(e)
    }
}

#[cfg(all(feature = "nyquest-multipart", feature = "nyquest-async"))]
#[compio::test]
async fn multipart_form() {
    use http_body_util::BodyExt;

    let form = nyquest::r#async::Body::multipart([nyquest::Part::new_with_content_type(
        "text",
        "text/plain",
        nyquest::PartBody::text("ttt"),
    )]);

    let server = server::http(move |mut req: http::Request<axum::body::Body>| async move {
        assert_eq!(req.method(), "POST");

        let mut full: Vec<u8> = Vec::new();
        while let Some(item) = req.body_mut().frame().await {
            full.extend(&*item.unwrap().into_data().unwrap());
        }

        full
    })
    .await;

    let url = format!("http://{}/multipart/1", server.addr());

    let client = builder().build_async().await.unwrap();

    let res = client
        .request(Request::post(url).with_body(form))
        .await
        .unwrap();

    let text = res.text().await.unwrap();
    assert!(text.contains(
        "Content-Disposition: form-data; name=\"text\"\r\ncontent-type: text/plain\r\n\r\nttt\r\n"
    ));
}

#[cfg(feature = "nyquest-async")]
#[compio::test]
async fn proxy_options_custom_http() {
    let proxy = server::http(move |req: axum::extract::Request| async move {
        assert_eq!(req.uri().to_string(), "http://cyper.local/prox");
        "proxied"
    })
    .await;

    let client = builder()
        .custom_proxy(CustomProxy::http(format!("http://{}", proxy.addr())))
        .no_redirects()
        .build_async()
        .await
        .unwrap();

    let text = client
        .request(Request::get("http://cyper.local/prox"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!("proxied", text);
}

#[cfg(feature = "nyquest-async")]
#[compio::test]
async fn proxy_options_custom_http_bypass() {
    let proxy = server::http(move |_req: axum::extract::Request| async move { "proxied" }).await;
    let target = server::http(move |_req: axum::extract::Request| async move { "direct" }).await;

    let client = builder()
        .custom_proxy(
            CustomProxy::http(format!("http://{}", proxy.addr())).with_bypass("127.0.0.1"),
        )
        .no_redirects()
        .build_async()
        .await
        .unwrap();

    let text = client
        .request(Request::get(format!("http://{}/", target.addr())))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!("direct", text);
}

#[cfg(feature = "nyquest-async")]
#[compio::test]
async fn proxy_options_https_only_keeps_http_direct() {
    let proxy = server::http(move |_req: axum::extract::Request| async move { "proxied" }).await;
    let target = server::http(move |_req: axum::extract::Request| async move { "direct" }).await;

    let client = builder()
        .custom_proxy(CustomProxy::https(format!("http://{}", proxy.addr())))
        .no_redirects()
        .build_async()
        .await
        .unwrap();

    let text = client
        .request(Request::get(format!("http://{}/", target.addr())))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!("direct", text);
}

#[cfg(feature = "nyquest-async")]
#[compio::test]
async fn max_response_buffer_size() {
    let server = server::http(move |_req: axum::extract::Request| async move { "Hello" }).await;

    let client = builder()
        .max_response_buffer_size(4)
        .build_async()
        .await
        .unwrap();

    let err = client
        .request(Request::get(format!("http://{}/", server.addr())))
        .await
        .unwrap()
        .text()
        .await
        .unwrap_err();
    assert!(matches!(err, nyquest::Error::ResponseTooLarge));
}

#[cfg(feature = "nyquest-async")]
#[compio::test]
async fn client_options_headers_and_base_url() {
    let server = server::http(move |req: axum::extract::Request| async move {
        let user_agents = req
            .headers()
            .get_all(http::header::USER_AGENT)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>();
        let default_header = req
            .headers()
            .get("x-default")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        format!(
            "{}|{}|{}|{}",
            user_agents.len(),
            user_agents.first().copied().unwrap_or_default(),
            default_header,
            req.uri()
        )
    })
    .await;

    let client = builder()
        .base_url(format!("http://{}", server.addr()))
        .user_agent("nyquest-test")
        .with_header("x-default", "yes")
        .no_proxy()
        .no_redirects()
        .build_async()
        .await
        .unwrap();

    let text = client
        .request(Request::get("/echo"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!("1|nyquest-test|yes|/echo", text);

    let text = client
        .request(Request::get("/echo").with_header("user-agent", "override"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!("1|override|yes|/echo", text);
}

#[cfg(feature = "nyquest-async")]
#[compio::test]
async fn cookies_are_kept_between_requests() {
    let server = server::http(move |req: axum::extract::Request| async move {
        if req.uri() == "/set" {
            http::Response::builder()
                .header(http::header::SET_COOKIE, "key=val")
                .body(axum::body::Body::empty())
                .unwrap()
        } else {
            let cookie = req
                .headers()
                .get(http::header::COOKIE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            http::Response::builder()
                .body(axum::body::Body::from(cookie))
                .unwrap()
        }
    })
    .await;

    let client = builder().build_async().await.unwrap();
    let base = format!("http://{}", server.addr());

    client
        .request(Request::get(format!("{base}/set")))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    let text = client
        .request(Request::get(format!("{base}/cookie")))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!("key=val", text);
}
