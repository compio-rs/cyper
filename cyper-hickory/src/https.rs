use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
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
use http::{Request, Uri};
use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, rt::ReadBufCursor};
use hyper_util::client::legacy::{
    Client,
    connect::{Connected, Connection},
};
use send_wrapper::SendWrapper;
use tower_service::Service;

use crate::CompioRuntimeProvider;

pub async fn connect_https(
    server_name: Arc<str>,
    path: Arc<str>,
    remote_addr: SocketAddr,
    bind_addr: Option<SocketAddr>,
    tls: ClientConfig,
    timeout: Duration,
) -> Result<DnsExchange<CompioRuntimeProvider>, NetError> {
    let connector = Connector::new(server_name.clone(), remote_addr, bind_addr, tls, timeout);

    let client = hyper_util::client::legacy::Client::builder(CompioExecutor)
        .http2_only(true)
        .set_host(true)
        .timer(CompioTimer)
        .build(connector);
    let stream = RequestSender::new(client, server_name, path);
    let (exchange, bg) = DnsExchange::from_stream(stream);
    compio::runtime::spawn(bg).detach();
    Ok(exchange)
}

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

        let request = crate::build_request(&server_name, &path, bytes.len())?;

        let (parts, ()) = request.into_parts();
        let request = Request::from_parts(parts, Full::new(Bytes::from(bytes)));

        let response = client
            .request(request)
            .await
            .map_err(|e| NetError::from(format!("request error: {e:?}")))?;
        let (response, body) = response.into_parts();

        let content_length = crate::get_content_length(&response.headers)?;

        let response_bytes = body
            .collect()
            .await
            .map_err(|e| NetError::from(format!("read response body error: {e:?}")))?
            .to_bytes()
            .to_vec();

        crate::build_response(response, content_length, response_bytes)
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
    timeout: Duration,
}

impl Connector {
    pub fn new(
        server_name: Arc<str>,
        remote_addr: SocketAddr,
        bind_addr: Option<SocketAddr>,
        tls: ClientConfig,
        timeout: Duration,
    ) -> Self {
        Self {
            server_name,
            remote_addr,
            bind_addr,
            tls: Arc::new(tls),
            timeout,
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
        let timeout = self.timeout;
        Box::pin(SendWrapper::new(async move {
            let stream = crate::connect_tcp(remote_addr, bind_addr, Some(timeout)).await?;
            let stream = TlsConnector::from(tls)
                .connect(&server_name, stream)
                .await?;
            Ok(HttpStream::new(stream))
        }))
    }
}

struct HttpStream {
    inner: HyperStream<TcpStream>,
}

impl HttpStream {
    pub fn new(stream: TlsStream<TcpStream>) -> Self {
        Self {
            inner: HyperStream::new_tls(stream),
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
        Connected::new().negotiated_h2()
    }
}
