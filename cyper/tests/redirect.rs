mod server;

use axum::response::IntoResponse;
use cyper::{Client, redirect};
use http::{StatusCode, header::LOCATION};

#[compio::test]
async fn test_redirect_follow() {
    let server = server::http(move |req: axum::extract::Request| async move {
        match req.uri().path() {
            "/redirect301" => {
                (StatusCode::MOVED_PERMANENTLY, [(LOCATION, "/target")]).into_response()
            }
            "/redirect302" => (StatusCode::FOUND, [(LOCATION, "/target")]).into_response(),
            "/redirect303" => (StatusCode::SEE_OTHER, [(LOCATION, "/target")]).into_response(),
            "/redirect307" => {
                (StatusCode::TEMPORARY_REDIRECT, [(LOCATION, "/target")]).into_response()
            }
            "/redirect308" => {
                (StatusCode::PERMANENT_REDIRECT, [(LOCATION, "/target")]).into_response()
            }
            "/target" => (StatusCode::OK, "target").into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    })
    .await;

    let client = Client::new();

    for path in [
        "/redirect301",
        "/redirect302",
        "/redirect303",
        "/redirect307",
        "/redirect308",
    ] {
        let res = client
            .get(format!("http://{}{path}", server.addr()))
            .unwrap()
            .send()
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK, "failed for {path}");
        assert_eq!(res.url().path(), "/target", "failed for {path}");
        let text = res.text().await.unwrap();
        assert_eq!(text, "target", "failed for {path}");
    }
}

#[compio::test]
async fn test_redirect_chain() {
    let server = server::http(move |req: axum::extract::Request| async move {
        match req.uri().path() {
            "/step/1" => (StatusCode::MOVED_PERMANENTLY, [(LOCATION, "/step/2")]).into_response(),
            "/step/2" => (StatusCode::MOVED_PERMANENTLY, [(LOCATION, "/step/3")]).into_response(),
            "/step/3" => (StatusCode::OK, "done").into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    })
    .await;

    let client = Client::new();
    let res = client
        .get(format!("http://{}/step/1", server.addr()))
        .unwrap()
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.url().path(), "/step/3");
    let text = res.text().await.unwrap();
    assert_eq!(text, "done");
}

#[compio::test]
async fn test_redirect_limit() {
    // Creates a loop: /loop redirects to /loop
    let server = server::http(move |req: axum::extract::Request| async move {
        match req.uri().path() {
            "/loop" => (StatusCode::MOVED_PERMANENTLY, [(LOCATION, "/loop")]).into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    })
    .await;

    let client = Client::builder()
        .redirect(redirect::Policy::limited(5))
        .build();

    let err = client
        .get(format!("http://{}/loop", server.addr()))
        .unwrap()
        .send()
        .await
        .unwrap_err();

    assert!(matches!(err, cyper::Error::Redirect(_)));
    assert!(err.to_string().contains("too many redirects"));
}

#[compio::test]
async fn test_no_redirect() {
    let server = server::http(move |_req: axum::extract::Request| async move {
        (StatusCode::MOVED_PERMANENTLY, [(LOCATION, "/target")]).into_response()
    })
    .await;

    let client = Client::builder().redirect(redirect::Policy::none()).build();

    let res = client
        .get(format!("http://{}/redirect", server.addr()))
        .unwrap()
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
}

#[compio::test]
async fn test_redirect_custom_policy() {
    let server = server::http(move |req: axum::extract::Request| async move {
        match req.uri().path() {
            "/redirect" => (StatusCode::FOUND, [(LOCATION, "/target")]).into_response(),
            "/target" => (StatusCode::OK, "target").into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    })
    .await;

    let client = Client::builder()
        .redirect(redirect::Policy::custom(|attempt| {
            if attempt.url().path() == "/target" {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build();

    // Should stop at the redirect because our custom policy
    // stops when the target is /target
    let res = client
        .get(format!("http://{}/redirect", server.addr()))
        .unwrap()
        .send()
        .await
        .unwrap();

    // Policy checked the redirect TO /target, and said stop.
    // So we get back the redirect response itself.
    assert_eq!(res.status(), StatusCode::FOUND);
}

#[compio::test]
async fn test_redirect_with_query() {
    let server = server::http(move |req: axum::extract::Request| async move {
        if req.uri().path() == "/redirect" {
            (
                StatusCode::MOVED_PERMANENTLY,
                [(LOCATION, "/target?foo=bar")],
            )
                .into_response()
        } else {
            let body = format!("query={:?}", req.uri().query());
            (StatusCode::OK, body).into_response()
        }
    })
    .await;

    let client = Client::new();
    let res = client
        .get(format!("http://{}/redirect", server.addr()))
        .unwrap()
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.url().query(), Some("foo=bar"));
}

#[compio::test]
async fn test_redirect_relative_url() {
    let server = server::http(move |req: axum::extract::Request| async move {
        match req.uri().path() {
            "/a" => (StatusCode::MOVED_PERMANENTLY, [(LOCATION, "/a/b")]).into_response(),
            "/a/b" => (StatusCode::MOVED_PERMANENTLY, [(LOCATION, "/a/b/c")]).into_response(),
            "/a/b/c" => (StatusCode::OK, "deep").into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    })
    .await;

    let client = Client::new();
    let res = client
        .get(format!("http://{}/a", server.addr()))
        .unwrap()
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.url().path(), "/a/b/c");
    let text = res.text().await.unwrap();
    assert_eq!(text, "deep");
}

#[compio::test]
async fn test_redirect_body_for_307() {
    let server = server::http(move |req: axum::extract::Request| async move {
        match req.uri().path() {
            "/redirect" => {
                (StatusCode::TEMPORARY_REDIRECT, [(LOCATION, "/target")]).into_response()
            }
            "/target" => {
                let body = axum::body::Bytes::from("ok");
                (StatusCode::OK, body).into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    })
    .await;

    let client = Client::new();
    let res = client
        .post(format!("http://{}/redirect", server.addr()))
        .unwrap()
        .body("hello")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let text = res.text().await.unwrap();
    assert_eq!(text, "ok");
}

#[compio::test]
async fn test_redirect_method_change_on_301() {
    let server = server::http(move |req: axum::extract::Request| async move {
        match req.uri().path() {
            "/redirect" => (StatusCode::MOVED_PERMANENTLY, [(LOCATION, "/target")]).into_response(),
            "/target" => {
                let method = req.method().to_string();
                (StatusCode::OK, method).into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    })
    .await;

    let client = Client::new();
    let res = client
        .post(format!("http://{}/redirect", server.addr()))
        .unwrap()
        .body("hello")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    // POST should change to GET on 301
    let text = res.text().await.unwrap();
    assert_eq!(text, "GET");
}
