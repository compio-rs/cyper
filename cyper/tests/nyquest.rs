use std::sync::Once;

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

#[cfg(feature = "nyquest-async")]
#[compio::test]
async fn response_text_async() {
    let server = server::http(move |_req| async { "Hello" }).await;

    register();

    let text = nyquest::r#async::get(format!("http://{}/text", server.addr()))
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
        register();

        let text = nyquest::blocking::get(format!("http://{}/text", server.addr()))
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
