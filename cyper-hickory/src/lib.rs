//! An adapter for using [`hickory_resolver`] with [`compio`] and
//! [`cyper_core`].

#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::{
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use compio::{
    BufResult,
    io::{compat::AsyncStream, util::Splittable},
    net::{TcpSocket, TcpStream, UdpSocket},
    runtime::Runtime,
};
use futures_util::{AsyncRead, AsyncWrite};
use hickory_net::{
    NetError,
    runtime::{DnsTcpStream, DnsUdpSocket, RuntimeProvider, Spawn, Time},
    xfer::DnsExchange,
};
use hickory_resolver::{
    ConnectionProvider, PoolContext,
    config::{ConnectionConfig, ProtocolConfig},
};
use send_wrapper::SendWrapper;

#[cfg(feature = "tls")]
mod tls;

#[cfg(feature = "https")]
mod https;

/// [`RuntimeProvider`] implementation for [`compio`]. It should not be used
/// directly. Instead, use [`CompioConnectionProvider`] which wraps this
/// provider and implements [`ConnectionProvider`] for hickory.
#[derive(Clone)]
pub struct CompioRuntimeProvider {
    runtime: SendWrapper<Runtime>,
}

impl CompioRuntimeProvider {
    /// Create a new [`CompioRuntimeProvider`] with the given [`Runtime`].
    pub fn new(runtime: Runtime) -> Self {
        Self {
            runtime: SendWrapper::new(runtime),
        }
    }

    /// Create a new [`CompioRuntimeProvider`] with the current runtime. It will
    /// panic if there is no current runtime.
    pub fn new_current() -> Self {
        Self::new(Runtime::current())
    }
}

impl RuntimeProvider for CompioRuntimeProvider {
    type Handle = CompioHandle;
    type Tcp = CompioStream<TcpStream>;
    type Timer = CompioTimer;
    type Udp = CompioUdpSocket;

    fn create_handle(&self) -> Self::Handle {
        CompioHandle::new(self.runtime.clone())
    }

    fn connect_tcp(
        &self,
        server_addr: SocketAddr,
        bind_addr: Option<SocketAddr>,
        timeout: Option<Duration>,
    ) -> Pin<Box<dyn Send + Future<Output = io::Result<Self::Tcp>>>> {
        Box::pin(SendWrapper::new(async move {
            let fut = async move {
                let stream = connect_tcp(server_addr, bind_addr).await?;
                Ok(CompioStream::new(stream))
            };
            if let Some(timeout) = timeout {
                compio::time::timeout(timeout, fut)
                    .await
                    .map_err(|_| io::ErrorKind::TimedOut.into())
                    .flatten()
            } else {
                fut.await
            }
        }))
    }

    fn bind_udp(
        &self,
        local_addr: SocketAddr,
        server_addr: SocketAddr,
    ) -> Pin<Box<dyn Send + Future<Output = io::Result<Self::Udp>>>> {
        Box::pin(SendWrapper::new(async move {
            let socket = UdpSocket::bind(local_addr).await?;
            socket.connect(server_addr).await?;
            Ok(CompioUdpSocket::new(socket))
        }))
    }
}

/// The handle for compio runtime.
#[derive(Clone)]
pub struct CompioHandle {
    runtime: SendWrapper<Runtime>,
}

impl CompioHandle {
    fn new(runtime: SendWrapper<Runtime>) -> Self {
        Self { runtime }
    }
}

impl Spawn for CompioHandle {
    fn spawn_bg(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        self.runtime.spawn(future).detach();
    }
}

/// The timer for compio runtime.
pub struct CompioTimer;

#[async_trait]
impl Time for CompioTimer {
    async fn delay_for(duration: Duration) {
        SendWrapper::new(compio::time::sleep(duration)).await;
    }

    async fn timeout<F: 'static + Future + Send>(
        duration: Duration,
        future: F,
    ) -> io::Result<F::Output> {
        SendWrapper::new(compio::time::timeout(duration, future))
            .await
            .map_err(|_| io::ErrorKind::TimedOut.into())
    }
}

/// [`DnsTcpStream`] implementation for compio's stream.
pub struct CompioStream<S: Splittable> {
    inner: SendWrapper<Pin<Box<AsyncStream<S>>>>,
}

impl<S: Splittable> CompioStream<S> {
    fn new(stream: S) -> Self {
        Self {
            inner: SendWrapper::new(Box::pin(AsyncStream::new(stream))),
        }
    }
}

impl<S: Splittable + 'static> DnsTcpStream for CompioStream<S>
where
    S::ReadHalf: compio::io::AsyncRead + Unpin,
    S::WriteHalf: compio::io::AsyncWrite + Unpin,
{
    type Time = CompioTimer;
}

impl<S: Splittable + 'static> AsyncRead for CompioStream<S>
where
    S::ReadHalf: compio::io::AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        unsafe { self.map_unchecked_mut(|this| &mut *this.inner) }.poll_read(cx, buf)
    }
}

impl<S: Splittable + 'static> AsyncWrite for CompioStream<S>
where
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

/// [`DnsUdpSocket`] implementation for compio's [`UdpSocket`].
pub struct CompioUdpSocket {
    inner: SendWrapper<UdpSocket>,
}

impl CompioUdpSocket {
    fn new(socket: UdpSocket) -> Self {
        Self {
            inner: SendWrapper::new(socket),
        }
    }
}

#[async_trait]
impl DnsUdpSocket for CompioUdpSocket {
    type Time = CompioTimer;

    fn poll_recv_from(
        &self,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        unreachable!("call recv_from directly instead of poll_recv_from")
    }

    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        SendWrapper::new(async {
            let buffer = Vec::with_capacity(buf.len());
            let BufResult(res, buffer) = self.inner.recv_from(buffer).await;
            let (written, addr) = res?;
            debug_assert_eq!(written, buffer.len());
            buf[..written].copy_from_slice(&buffer);
            Ok((written, addr))
        })
        .await
    }

    fn poll_send_to(
        &self,
        _cx: &mut Context<'_>,
        _buf: &[u8],
        _target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        unreachable!("call send_to directly instead of poll_send_to")
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        SendWrapper::new(async {
            let buffer = buf.to_vec();
            let BufResult(res, _) = self.inner.send_to(buffer, target).await;
            let written = res?;
            Ok(written)
        })
        .await
    }
}

/// The [`ConnectionProvider`] implementation for compio runtime. It should be
/// used to create [`Resolver`](hickory_resolver::Resolver) for compio runtime.
///
/// This provider should be created by [`CompioConnectionProvider::default`]
/// inside a compio runtime. If you're not inside a compio runtime, call
/// [`Runtime::enter`] to enter a compio runtime first, then create the
/// provider.
#[derive(Clone)]
pub struct CompioConnectionProvider {
    provider: CompioRuntimeProvider,
}

impl Default for CompioConnectionProvider {
    fn default() -> Self {
        Self {
            provider: CompioRuntimeProvider::new_current(),
        }
    }
}

impl CompioConnectionProvider {
    /// Create a new [`CompioConnectionProvider`] with the given [`Runtime`].
    pub fn new(runtime: Runtime) -> Self {
        Self {
            provider: CompioRuntimeProvider::new(runtime),
        }
    }

    /// Create a new [`CompioConnectionProvider`] with the current runtime. It
    /// will panic if there is no current runtime.
    pub fn new_current() -> Self {
        Self {
            provider: CompioRuntimeProvider::new_current(),
        }
    }
}

impl ConnectionProvider for CompioConnectionProvider {
    type Conn = DnsExchange<CompioRuntimeProvider>;
    type FutureConn = Pin<Box<dyn Future<Output = Result<Self::Conn, NetError>> + Send + 'static>>;
    type RuntimeProvider = CompioRuntimeProvider;

    fn new_connection(
        &self,
        ip: IpAddr,
        config: &ConnectionConfig,
        cx: &PoolContext,
    ) -> Result<Self::FutureConn, hickory_net::NetError> {
        match &config.protocol {
            ProtocolConfig::Udp | ProtocolConfig::Tcp => {
                self.provider.new_connection(ip, config, cx)
            }
            #[cfg(feature = "tls")]
            ProtocolConfig::Tls { server_name } => {
                let server_name = server_name.clone();
                let remote_addr = SocketAddr::new(ip, config.port);
                let bind_addr = config.bind_addr;
                let tls = cx.tls.clone();
                Ok(Box::pin(SendWrapper::new(async move {
                    tls::connect_tls(&server_name, remote_addr, bind_addr, tls).await
                })))
            }
            #[cfg(feature = "https")]
            ProtocolConfig::Https { server_name, path } => {
                let server_name = server_name.clone();
                let path = path.clone();
                let remote_addr = SocketAddr::new(ip, config.port);
                let bind_addr = config.bind_addr;
                let tls = cx.tls.clone();
                Ok(Box::pin(SendWrapper::new(async move {
                    https::connect_https(server_name, path, remote_addr, bind_addr, tls).await
                })))
            }
            #[allow(unreachable_patterns)]
            _ => Err(NetError::from("protocol config not supported")),
        }
    }

    fn runtime_provider(&self) -> &Self::RuntimeProvider {
        &self.provider
    }
}

async fn connect_tcp(
    server_addr: SocketAddr,
    bind_addr: Option<SocketAddr>,
) -> io::Result<TcpStream> {
    if let Some(bind_addr) = bind_addr {
        let socket = if bind_addr.is_ipv4() {
            TcpSocket::new_v4().await?
        } else {
            TcpSocket::new_v6().await?
        };
        socket.bind(bind_addr).await?;
        socket.connect(server_addr).await
    } else {
        TcpStream::connect(server_addr).await
    }
}
