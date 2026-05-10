use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use compio::{
    io::util::Splittable,
    net::{TcpSocket, TcpStream},
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

use crate::{CompioRuntimeProvider, CompioTimer};

pub struct CompioTlsStream<S: Splittable> {
    inner: SendWrapper<TlsStream<S>>,
}

impl<S: Splittable> CompioTlsStream<S> {
    fn new(stream: TlsStream<S>) -> Self {
        Self {
            inner: SendWrapper::new(stream),
        }
    }
}

impl<S: Splittable + 'static> DnsTcpStream for CompioTlsStream<S>
where
    S::ReadHalf: compio::io::AsyncRead + Unpin,
    S::WriteHalf: compio::io::AsyncWrite + Unpin,
{
    type Time = CompioTimer;
}

impl<S: Splittable + 'static> AsyncRead for CompioTlsStream<S>
where
    S::ReadHalf: compio::io::AsyncRead + Unpin,
    S::WriteHalf: compio::io::AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        unsafe { self.map_unchecked_mut(|this| &mut *this.inner) }.poll_read(cx, buf)
    }
}

impl<S: Splittable + 'static> AsyncWrite for CompioTlsStream<S>
where
    S::ReadHalf: compio::io::AsyncRead + Unpin,
    S::WriteHalf: compio::io::AsyncWrite + Unpin,
{
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
    server_name: &str,
    remote_addr: SocketAddr,
    bind_addr: Option<SocketAddr>,
    tls: ClientConfig,
) -> Result<DnsExchange<CompioRuntimeProvider>, NetError> {
    let stream = if let Some(bind_addr) = bind_addr {
        let socket = if bind_addr.is_ipv4() {
            TcpSocket::new_v4().await?
        } else {
            TcpSocket::new_v6().await?
        };
        socket.bind(bind_addr).await?;
        socket.connect(remote_addr).await?
    } else {
        TcpStream::connect(remote_addr).await?
    };
    let remote_addr = stream.peer_addr()?;
    let stream = TlsConnector::from(Arc::new(tls))
        .connect(server_name, stream)
        .await?;
    let (stream, handle) =
        hickory_net::tcp::TcpStream::from_stream(CompioTlsStream::new(stream), remote_addr);
    let multiplexer = DnsMultiplexer::new(
        hickory_net::tcp::TcpClientStream::from_stream(stream),
        handle,
    );
    let (exchange, background) = DnsExchange::from_stream(multiplexer);
    compio::runtime::spawn(background).detach();
    Ok(exchange)
}
