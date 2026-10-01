use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use compio::{
    rustls::ClientConfig,
    tls::{TlsConnector, TlsStream},
};
use futures_util::{AsyncRead, AsyncWrite};
use hickory_net::{
    NetError,
    runtime::DnsTcpStream,
    xfer::{DnsExchange, DnsMultiplexer},
};
use send_wrapper::SendWrapper;

use crate::{CompioRuntimeProvider, CompioTimer, connect_tcp};

pub struct CompioTlsStream<S> {
    inner: SendWrapper<TlsStream<S>>,
}

impl<S> CompioTlsStream<S> {
    fn new(stream: TlsStream<S>) -> Self {
        Self {
            inner: SendWrapper::new(stream),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + 'static> DnsTcpStream for CompioTlsStream<S> {
    type Time = CompioTimer;
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for CompioTlsStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        unsafe { self.map_unchecked_mut(|this| &mut *this.inner) }.poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for CompioTlsStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        unsafe { self.map_unchecked_mut(|this| &mut *this.inner) }.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        unsafe { self.map_unchecked_mut(|this| &mut *this.inner) }.poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        unsafe { self.map_unchecked_mut(|this| &mut *this.inner) }.poll_close(cx)
    }
}

pub async fn connect_tls(
    server_name: Arc<str>,
    remote_addr: SocketAddr,
    bind_addr: Option<SocketAddr>,
    tls: ClientConfig,
    timeout: Duration,
    max_active_requests: usize,
) -> Result<DnsExchange<CompioRuntimeProvider>, NetError> {
    let stream = connect_tcp(remote_addr, bind_addr, Some(timeout)).await?;
    let remote_addr = stream.peer_addr()?;
    let stream = TlsConnector::from(Arc::new(tls))
        .connect(&server_name, stream.into_poll_fd()?)
        .await?;
    let (stream, handle) =
        hickory_net::tcp::TcpStream::from_stream(CompioTlsStream::new(stream), remote_addr);
    let multiplexer = DnsMultiplexer::new(
        hickory_net::tcp::TcpClientStream::from_stream(stream),
        handle,
    )
    .with_max_active_requests(max_active_requests);
    let (exchange, background) = DnsExchange::from_stream(multiplexer);
    compio::runtime::spawn(background).detach();
    Ok(exchange)
}
