use std::{
    net::SocketAddr,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use compio::{
    bytes::{Buf, Bytes},
    rustls::ClientConfig,
};
use compio_log::debug;
use futures_util::Stream;
use hickory_net::{
    NetError,
    proto::op::{DnsRequest, DnsResponse},
    xfer::{DnsExchange, DnsRequestSender, DnsResponseStream},
};
use http::{Request, Uri, uri};
use send_wrapper::SendWrapper;

use crate::{CompioRuntimeProvider, MIME_APPLICATION_DNS};

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

    compio::runtime::spawn(async move {
        driver.wait_idle().await;
    })
    .detach();

    let stream = H3RequestSender::new(send_request, server_name, path);
    let (exchange, bg) = DnsExchange::from_stream(stream);
    compio::runtime::spawn(bg).detach();
    Ok(exchange)
}

type SendRequest = compio::quic::h3::client::SendRequest<compio::quic::h3::OpenStreams, Bytes>;

struct H3RequestSender {
    send_request: SendWrapper<SendRequest>,
    server_name: Arc<str>,
    path: Arc<str>,
    is_shutdown: bool,
}

impl H3RequestSender {
    fn new(send_request: SendRequest, server_name: Arc<str>, path: Arc<str>) -> Self {
        Self {
            send_request: SendWrapper::new(send_request),
            server_name,
            path,
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

        let mut parts = uri::Parts::default();
        parts.path_and_query = Some(
            uri::PathAndQuery::from_str(&path)
                .map_err(|e| NetError::from(format!("invalid DoH3 path: {e}")))?,
        );
        parts.scheme = Some(uri::Scheme::HTTPS);
        parts.authority = Some(
            uri::Authority::from_str(&server_name)
                .map_err(|e| NetError::from(format!("invalid authority: {e}")))?,
        );

        let url =
            Uri::from_parts(parts).map_err(|e| NetError::from(format!("uri parse error: {e}")))?;

        let request = Request::builder()
            .method("POST")
            .uri(url)
            .header(http::header::CONTENT_TYPE, MIME_APPLICATION_DNS)
            .header(http::header::ACCEPT, MIME_APPLICATION_DNS)
            .header(http::header::CONTENT_LENGTH, bytes.len())
            .body(())
            .map_err(|e| NetError::from(format!("build h3 request error: {e}")))?;

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

        debug!("got response: {:#?}", resp);

        let content_length = resp
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .map(|v| v.to_str())
            .transpose()
            .map_err(|e| NetError::from(format!("bad headers received: {e}")))?
            .map(usize::from_str)
            .transpose()
            .map_err(|e| NetError::from(format!("bad headers received: {e}")))?;

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

        if let Some(content_length) = content_length
            && response_bytes.len() != content_length
        {
            return Err(NetError::from(format!(
                "expected byte length: {}, got: {}",
                content_length,
                response_bytes.len()
            )));
        }

        if !resp.status().is_success() {
            let error_string = String::from_utf8_lossy(response_bytes.as_ref());

            return Err(NetError::from(format!(
                "http unsuccessful code: {}, message: {}",
                resp.status(),
                error_string
            )));
        }

        let content_type = resp
            .headers()
            .get(http::header::CONTENT_TYPE)
            .map(|h| {
                h.to_str()
                    .map_err(|e| NetError::from(format!("ContentType header not a string: {e}")))
            })
            .unwrap_or(Ok(MIME_APPLICATION_DNS))?;

        if content_type != MIME_APPLICATION_DNS {
            return Err(NetError::from(format!(
                "ContentType unsupported (must be '{}'): '{}'",
                MIME_APPLICATION_DNS, content_type
            )));
        }

        DnsResponse::from_buffer(response_bytes).map_err(NetError::from)
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
    }

    fn is_shutdown(&self) -> bool {
        self.is_shutdown
    }
}

impl Stream for H3RequestSender {
    type Item = Result<(), NetError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_shutdown {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Ok(())))
        }
    }
}
