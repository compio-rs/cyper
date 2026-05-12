use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use compio::{
    bytes::{Buf, Bytes},
    rustls::ClientConfig,
};
use compio_log::{debug, warn};
use futures_channel::mpsc::Sender;
use futures_util::{FutureExt, SinkExt, Stream};
use hickory_net::{
    NetError,
    proto::op::{DnsRequest, DnsResponse},
    xfer::{DnsExchange, DnsRequestSender, DnsResponseStream},
};
use send_wrapper::SendWrapper;

use crate::CompioRuntimeProvider;

const H3_ALPN: &[u8] = b"h3";

pub async fn connect_h3(
    server_name: Arc<str>,
    path: Arc<str>,
    remote_addr: SocketAddr,
    bind_addr: Option<SocketAddr>,
    config: ClientConfig,
    enable_grease: bool,
    timeout: Duration,
) -> Result<DnsExchange<CompioRuntimeProvider>, NetError> {
    let conn = crate::connect_quic(
        server_name.clone(),
        remote_addr,
        bind_addr,
        config,
        timeout,
        H3_ALPN,
    )
    .await?;

    let (mut driver, send_request) = compio::quic::h3::client::builder()
        .send_grease(enable_grease)
        .build(conn)
        .await
        .map_err(|e| NetError::from(format!("h3 client error: {e}")))?;

    let (tx, mut rx) = futures_channel::mpsc::channel::<()>(1);

    compio::runtime::spawn(async move {
        futures_util::select! {
            error = driver.wait_idle().fuse() => {
                if !error.is_h3_no_error() {
                    warn!("h3 connection failed to close: {}", error);
                }
            }
            _ = rx.recv().fuse() => {
                debug!("h3 connection is shutting down: {}", remote_addr);
            }
        }
    })
    .detach();

    let stream = H3RequestSender::new(send_request, server_name, path, tx);
    let (exchange, bg) = DnsExchange::from_stream(stream);
    compio::runtime::spawn(bg).detach();
    Ok(exchange)
}

type SendRequest = compio::quic::h3::client::SendRequest<compio::quic::h3::OpenStreams, Bytes>;

struct H3RequestSender {
    send_request: SendWrapper<SendRequest>,
    server_name: Arc<str>,
    path: Arc<str>,
    tx: Sender<()>,
    is_shutdown: bool,
}

impl H3RequestSender {
    fn new(
        send_request: SendRequest,
        server_name: Arc<str>,
        path: Arc<str>,
        tx: Sender<()>,
    ) -> Self {
        Self {
            send_request: SendWrapper::new(send_request),
            server_name,
            path,
            tx,
            is_shutdown: false,
        }
    }

    async fn inner_send(
        mut send_request: SendWrapper<SendRequest>,
        server_name: Arc<str>,
        path: Arc<str>,
        mut request: DnsRequest,
    ) -> Result<DnsResponse, NetError> {
        request.metadata.id = 0;
        let bytes = request.to_vec()?;

        let request = crate::build_request(&server_name, &path, bytes.len())?;

        let mut stream = send_request
            .send_request(request)
            .await
            .map_err(|e| NetError::from(format!("h3 send request error: {e}")))?;

        stream
            .send_data(Bytes::from(bytes))
            .await
            .map_err(|e| NetError::from(format!("h3 send data error: {e}")))?;

        stream
            .finish()
            .await
            .map_err(|e| NetError::from(format!("h3 finish error: {e}")))?;

        let resp = stream
            .recv_response()
            .await
            .map_err(|e| NetError::from(format!("h3 recv response error: {e}")))?;
        let (resp, ()) = resp.into_parts();

        let content_length = crate::get_content_length(&resp.headers)?;

        let mut response_bytes =
            Vec::with_capacity(content_length.unwrap_or(512).clamp(512, 4_096));
        while let Some(chunk) = stream
            .recv_data()
            .await
            .map_err(|e| NetError::from(format!("h3 recv data error: {e}")))?
        {
            response_bytes.extend_from_slice(chunk.chunk());

            if let Some(content_length) = content_length
                && response_bytes.len() >= content_length
            {
                break;
            }
        }

        crate::build_response(resp, content_length, response_bytes)
    }
}

impl DnsRequestSender for H3RequestSender {
    fn send_message(&mut self, request: DnsRequest) -> DnsResponseStream {
        if self.is_shutdown {
            panic!("can not send messages after stream is shutdown")
        }

        Box::pin(SendWrapper::new(Self::inner_send(
            self.send_request.clone(),
            self.server_name.clone(),
            self.path.clone(),
            request,
        )))
        .into()
    }

    fn shutdown(&mut self) {
        self.is_shutdown = true;
        compio::runtime::spawn({
            let mut tx = self.tx.clone();
            async move {
                let _ = tx.send(()).await;
            }
        })
        .detach();
    }

    fn is_shutdown(&self) -> bool {
        self.is_shutdown
    }
}

impl Stream for H3RequestSender {
    type Item = Result<(), NetError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_shutdown {
            return Poll::Ready(None);
        }

        if self.tx.is_closed() {
            return Poll::Ready(Some(Err(NetError::from(
                "h3 connection is already shutdown",
            ))));
        }

        Poll::Ready(Some(Ok(())))
    }
}
