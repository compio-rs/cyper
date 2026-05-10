use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use compio::{
    BufResult,
    io::compat::AsyncStream,
    net::{TcpSocket, TcpStream, UdpSocket},
    runtime::Runtime,
};
use futures_util::{AsyncRead, AsyncWrite};
use hickory_net::runtime::{DnsTcpStream, DnsUdpSocket, RuntimeProvider, Spawn, Time};
use send_wrapper::SendWrapper;

#[derive(Clone)]
pub struct CompioRuntimeProvider {
    runtime: SendWrapper<Runtime>,
}

impl CompioRuntimeProvider {
    pub fn new(runtime: Runtime) -> Self {
        Self {
            runtime: SendWrapper::new(runtime),
        }
    }

    pub fn new_current() -> Self {
        Self::new(Runtime::current())
    }
}

impl RuntimeProvider for CompioRuntimeProvider {
    type Handle = CompioHandle;
    type Tcp = CompioTcpStream;
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
                let stream = if let Some(bind_addr) = bind_addr {
                    let socket = if bind_addr.is_ipv4() {
                        TcpSocket::new_v4().await?
                    } else {
                        TcpSocket::new_v6().await?
                    };
                    socket.bind(bind_addr).await?;
                    socket.connect(server_addr).await?
                } else {
                    TcpStream::connect(server_addr).await?
                };
                Ok(CompioTcpStream::new(stream))
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

pub struct CompioTcpStream {
    inner: SendWrapper<Pin<Box<AsyncStream<TcpStream>>>>,
}

impl CompioTcpStream {
    fn new(stream: TcpStream) -> Self {
        Self {
            inner: SendWrapper::new(Box::pin(AsyncStream::new(stream))),
        }
    }
}

impl DnsTcpStream for CompioTcpStream {
    type Time = CompioTimer;
}

impl AsyncRead for CompioTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        unsafe { self.map_unchecked_mut(|this| &mut *this.inner) }.poll_read(cx, buf)
    }
}

impl AsyncWrite for CompioTcpStream {
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
        todo!()
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
        todo!()
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
