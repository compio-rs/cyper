use std::{
    borrow::Cow,
    io,
    ops::DerefMut,
    pin::Pin,
    task::{Context, Poll, ready},
};

use ambassador::{Delegate, delegatable_trait_remote};
#[cfg(any(feature = "native-tls", feature = "rustls"))]
use compio::tls::TlsStream;
use compio::{
    buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut},
    io::{AsyncRead, AsyncWrite, compat::AsyncStream},
    net::TcpStream,
};
use hyper::Uri;
#[cfg(feature = "client")]
use hyper_util::client::legacy::connect::{Connected, Connection};
use send_wrapper::SendWrapper;

use crate::TlsBackend;

#[delegatable_trait_remote]
trait AsyncWrite {
    async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T>;
    async fn write_vectored<T: IoVectoredBuf>(&mut self, buf: T) -> BufResult<usize, T>;
    async fn flush(&mut self) -> io::Result<()>;
    async fn shutdown(&mut self) -> io::Result<()>;
}

#[delegatable_trait_remote]
trait AsyncRead {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B>;
    async fn read_vectored<V: IoVectoredBufMut>(&mut self, buf: V) -> BufResult<usize, V>;
}

#[allow(clippy::large_enum_variant)]
#[derive(Delegate)]
#[delegate(AsyncRead)]
#[delegate(AsyncWrite)]
enum HttpStreamInner {
    Tcp(TcpStream),
    #[cfg(any(feature = "native-tls", feature = "rustls"))]
    Tls(TlsStream<TcpStream>),
}

impl HttpStreamInner {
    pub async fn connect(uri: Uri, tls: TlsBackend) -> io::Result<Self> {
        let scheme = uri.scheme_str().unwrap_or("http");
        let host = uri.host().expect("there should be host");
        let port = uri.port_u16();
        match scheme {
            "http" => {
                let stream = TcpStream::connect((host, port.unwrap_or(80))).await;
                stream.map(Self::Tcp)
            }
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            "https" => {
                let stream = TcpStream::connect((host, port.unwrap_or(443))).await?;
                let connector = tls.create_connector()?;
                connector.connect(host, stream).await.map(Self::Tls)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported scheme",
            )),
        }
    }

    pub fn from_tcp(s: TcpStream) -> Self {
        Self::Tcp(s)
    }

    #[cfg(any(feature = "native-tls", feature = "rustls"))]
    pub fn from_tls(s: TlsStream<TcpStream>) -> Self {
        Self::Tls(s)
    }

    fn negotiated_alpn(&self) -> Option<Cow<'_, [u8]>> {
        match self {
            Self::Tcp(_) => None,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.negotiated_alpn(),
        }
    }
}

/// A HTTP stream wrapper, based on compio, and exposes [`hyper::rt`]
/// interfaces.
pub struct HttpStream(HyperStream<HttpStreamInner>);

impl HttpStream {
    /// Create [`HttpStream`] with target uri and TLS backend.
    pub async fn connect(uri: Uri, tls: TlsBackend) -> io::Result<Self> {
        Ok(Self::from_inner(HttpStreamInner::connect(uri, tls).await?))
    }

    /// Create [`HttpStream`] with connected TCP stream.
    pub fn from_tcp(s: TcpStream) -> Self {
        Self::from_inner(HttpStreamInner::from_tcp(s))
    }

    /// Create [`HttpStream`] with connected TLS stream.
    #[cfg(any(feature = "native-tls", feature = "rustls"))]
    pub fn from_tls(s: TlsStream<TcpStream>) -> Self {
        Self::from_inner(HttpStreamInner::from_tls(s))
    }

    fn from_inner(s: HttpStreamInner) -> Self {
        Self(HyperStream::new(s))
    }
}

impl hyper::rt::Read for HttpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let inner = std::pin::pin!(&mut self.0);
        inner.poll_read(cx, buf)
    }
}

impl hyper::rt::Write for HttpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let inner = std::pin::pin!(&mut self.0);
        inner.poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner = std::pin::pin!(&mut self.0);
        inner.poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner = std::pin::pin!(&mut self.0);
        inner.poll_shutdown(cx)
    }
}

#[cfg(feature = "client")]
impl Connection for HttpStream {
    fn connected(&self) -> Connected {
        let conn = Connected::new();
        let is_h2 = self
            .0
            .0
            .get_ref()
            .negotiated_alpn()
            .is_some_and(|alpn| alpn.as_slice() == b"h2");
        if is_h2 { conn.negotiated_h2() } else { conn }
    }
}

/// A stream wrapper for hyper.
pub struct HyperStream<S>(SendWrapper<AsyncStream<S>>);

impl<S> HyperStream<S> {
    /// Create a hyper stream wrapper.
    pub fn new(s: S) -> Self {
        Self(SendWrapper::new(AsyncStream::new(s)))
    }

    /// Get the reference of the inner stream.
    pub fn get_ref(&self) -> &S {
        self.0.get_ref()
    }
}

impl<S> std::fmt::Debug for HyperStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HyperStream").finish_non_exhaustive()
    }
}

impl<S: AsyncRead + Unpin + 'static> hyper::rt::Read for HyperStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let stream = unsafe { self.map_unchecked_mut(|this| this.0.deref_mut()) };
        let slice = unsafe { buf.as_mut() };
        let len = ready!(stream.poll_read_uninit(cx, slice))?;
        unsafe { buf.advance(len) };
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin + 'static> hyper::rt::Write for HyperStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let stream = unsafe { self.map_unchecked_mut(|this| this.0.deref_mut()) };
        futures_util::AsyncWrite::poll_write(stream, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let stream = unsafe { self.map_unchecked_mut(|this| this.0.deref_mut()) };
        futures_util::AsyncWrite::poll_flush(stream, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let stream = unsafe { self.map_unchecked_mut(|this| this.0.deref_mut()) };
        futures_util::AsyncWrite::poll_close(stream, cx)
    }
}
