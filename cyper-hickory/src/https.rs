use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};

use compio::{
    net::TcpStream,
    rustls::ClientConfig,
    tls::{TlsConnector, TlsStream},
};
use cyper_core::{CompioExecutor, CompioTimer, HyperStream};
use futures_util::Stream;
use hickory_net::{
    NetError,
    proto::op::{DnsRequest, DnsResponse},
    xfer::{DnsExchange, DnsRequestSender, DnsResponseStream},
};
use http::{Request, Uri, uri};
use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, rt::ReadBufCursor};
use hyper_util::client::legacy::{
    Client,
    connect::{Connected, Connection},
};
use send_wrapper::SendWrapper;
use tower_service::Service;

use crate::{CompioRuntimeProvider, connect_tcp};

pub async fn connect_https(
    server_name: Arc<str>,
    path: Arc<str>,
    remote_addr: SocketAddr,
    bind_addr: Option<SocketAddr>,
    tls: ClientConfig,
) -> Result<DnsExchange<CompioRuntimeProvider>, NetError> {
    let connector = Connector::new(server_name.clone(), remote_addr, bind_addr, tls);

    let client = hyper_util::client::legacy::Client::builder(CompioExecutor)
        .set_host(true)
        .timer(CompioTimer)
        .build(connector);
    let stream = RequestSender::new(client, server_name, path);
    let (exchange, bg) = DnsExchange::from_stream(stream);
    compio::runtime::spawn(bg).detach();
    Ok(exchange)
}

const MIME_APPLICATION_DNS: &str = "application/dns-message";

struct RequestSender {
    client: Arc<Client<Connector, Full<Bytes>>>,
    server_name: Arc<str>,
    path: Arc<str>,
    is_shutdown: bool,
}

impl RequestSender {
    pub fn new(
        client: Client<Connector, Full<Bytes>>,
        server_name: Arc<str>,
        path: Arc<str>,
    ) -> Self {
        Self {
            client: Arc::new(client),
            server_name,
            path,
            is_shutdown: false,
        }
    }

    async fn send_message_impl(
        client: Arc<Client<Connector, Full<Bytes>>>,
        server_name: Arc<str>,
        path: Arc<str>,
        mut request: DnsRequest,
    ) -> Result<DnsResponse, NetError> {
        request.metadata.id = 0;
        let bytes = request.to_vec()?;

        let mut parts = uri::Parts::default();
        parts.path_and_query = Some(
            uri::PathAndQuery::from_str(&path)
                .map_err(|e| NetError::from(format!("invalid DoH path: {e}")))?,
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
            .body(Full::new(Bytes::from(bytes)))
            .map_err(|e| NetError::from(format!("build request error: {e}")))?;

        let response = client
            .request(request)
            .await
            .map_err(|e| NetError::from(format!("request error: {e}")))?;
        let (response, body) = response.into_parts();

        let content_length = response
            .headers
            .get(http::header::CONTENT_LENGTH)
            .map(|v| v.to_str())
            .transpose()
            .map_err(|e| NetError::from(format!("bad headers received: {e}")))?
            .map(usize::from_str)
            .transpose()
            .map_err(|e| NetError::from(format!("bad headers received: {e}")))?;

        let response_bytes = body
            .collect()
            .await
            .map_err(|e| NetError::from(format!("read response body error: {e}")))?
            .to_bytes();

        if let Some(content_length) = content_length
            && response_bytes.len() != content_length
        {
            return Err(NetError::from(format!(
                "expected byte length: {}, got: {}",
                content_length,
                response_bytes.len()
            )));
        }

        if !response.status.is_success() {
            let error_string = String::from_utf8_lossy(response_bytes.as_ref());

            return Err(NetError::from(format!(
                "http unsuccessful code: {}, message: {}",
                response.status, error_string
            )));
        } else {
            let content_type = response
                .headers
                .get(http::header::CONTENT_TYPE)
                .map(|h| {
                    h.to_str().map_err(|err| {
                        NetError::from(format!("ContentType header not a string: {err}"))
                    })
                })
                .unwrap_or(Ok(MIME_APPLICATION_DNS))?;

            if content_type != MIME_APPLICATION_DNS {
                return Err(NetError::from(format!(
                    "ContentType unsupported (must be '{}'): '{}'",
                    MIME_APPLICATION_DNS, content_type
                )));
            }
        }

        DnsResponse::from_buffer(response_bytes.to_vec()).map_err(NetError::from)
    }
}

impl DnsRequestSender for RequestSender {
    fn send_message(&mut self, request: DnsRequest) -> DnsResponseStream {
        if self.is_shutdown {
            panic!("can not send messages after stream is shutdown")
        }

        Box::pin(Self::send_message_impl(
            self.client.clone(),
            self.server_name.clone(),
            self.path.clone(),
            request,
        ))
        .into()
    }

    fn shutdown(&mut self) {
        self.is_shutdown = true;
    }

    fn is_shutdown(&self) -> bool {
        self.is_shutdown
    }
}

impl Stream for RequestSender {
    type Item = Result<(), NetError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_shutdown {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Ok(())))
        }
    }
}

#[derive(Clone)]
struct Connector {
    server_name: Arc<str>,
    remote_addr: SocketAddr,
    bind_addr: Option<SocketAddr>,
    tls: Arc<ClientConfig>,
}

impl Connector {
    pub fn new(
        server_name: Arc<str>,
        remote_addr: SocketAddr,
        bind_addr: Option<SocketAddr>,
        tls: ClientConfig,
    ) -> Self {
        Self {
            server_name,
            remote_addr,
            bind_addr,
            tls: Arc::new(tls),
        }
    }
}

impl Service<Uri> for Connector {
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;
    type Response = HttpStream;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Uri) -> Self::Future {
        let server_name = self.server_name.clone();
        let remote_addr = self.remote_addr;
        let bind_addr = self.bind_addr;
        let tls = self.tls.clone();
        Box::pin(SendWrapper::new(async move {
            let stream = connect_tcp(remote_addr, bind_addr).await?;
            let stream = TlsConnector::from(tls)
                .connect(&server_name, stream)
                .await?;
            Ok(HttpStream::new(stream))
        }))
    }
}

struct HttpStream {
    inner: HyperStream<TcpStream>,
    is_h2: bool,
}

impl HttpStream {
    pub fn new(stream: TlsStream<TcpStream>) -> Self {
        let is_h2 = stream
            .negotiated_alpn()
            .map(|alpn| *alpn == *b"h2")
            .unwrap_or_default();
        Self {
            inner: HyperStream::new_tls(stream),
            is_h2,
        }
    }
}

impl hyper::rt::Read for HttpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for HttpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl Connection for HttpStream {
    fn connected(&self) -> Connected {
        let conn = Connected::new();
        if self.is_h2 {
            conn.negotiated_h2()
        } else {
            conn
        }
    }
}
