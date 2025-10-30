mod server;

#[compio::test]
async fn response_text() {
    let server = server::http(move |_req| async { "Hello" }).await;

    cyper::nyquest::register();

    let text = nyquest::r#async::get(format!("http://{}/text", server.addr()))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!("Hello", text);
}
