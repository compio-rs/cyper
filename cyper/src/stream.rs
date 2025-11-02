use std::{
    borrow::Cow,
    io,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(any(feature = "native-tls", feature = "rustls"))]
use compio::tls::TlsStream;
use compio::{
    buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut},
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use cyper_core::HyperStream;
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};

use crate::{Error, Result, TlsBackend};

#[allow(clippy::large_enum_variant)]
enum HttpStreamInner {
    Tcp(TcpStream),
    #[cfg(any(feature = "native-tls", feature = "rustls"))]
    Tls(TlsStream<TcpStream>),
}

impl HttpStreamInner {
    pub async fn connect(uri: Uri, tls: TlsBackend) -> Result<Self> {
        let scheme = uri.scheme_str().unwrap_or("http");
        let host = uri.host().expect("there should be host");
        let port = uri.port_u16();
        match scheme {
            "http" => {
                let stream = TcpStream::connect((host, port.unwrap_or(80))).await?;
                // Ignore it.
                let _tls = tls;
                Ok(Self::Tcp(stream))
            }
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            "https" => {
                let stream = TcpStream::connect((host, port.unwrap_or(443))).await?;
                let connector = tls.create_connector()?;
                Ok(Self::Tls(connector.connect(host, stream).await?))
            }
            _ => Err(Error::BadScheme(scheme.to_string())),
        }
    }

    fn negotiated_alpn(&self) -> Option<Cow<'_, [u8]>> {
        match self {
            Self::Tcp(_) => None,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.negotiated_alpn(),
        }
    }
}

impl AsyncRead for HttpStreamInner {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            Self::Tcp(s) => s.read(buf).await,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.read(buf).await,
        }
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, buf: V) -> BufResult<usize, V> {
        match self {
            Self::Tcp(s) => s.read_vectored(buf).await,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.read_vectored(buf).await,
        }
    }
}

impl AsyncWrite for HttpStreamInner {
    async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        match self {
            Self::Tcp(s) => s.write(buf).await,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.write(buf).await,
        }
    }

    async fn write_vectored<T: IoVectoredBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        match self {
            Self::Tcp(s) => s.write_vectored(buf).await,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.write_vectored(buf).await,
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush().await,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.flush().await,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.shutdown().await,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.shutdown().await,
        }
    }
}

/// A HTTP stream wrapper, based on compio, and exposes [`hyper::rt`]
/// interfaces.
pub struct HttpStream(HyperStream<HttpStreamInner>);

impl HttpStream {
    /// Create [`HttpStream`] with target uri and TLS backend.
    pub async fn connect(uri: Uri, tls: TlsBackend) -> Result<Self> {
        Ok(Self::from_inner(HttpStreamInner::connect(uri, tls).await?))
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

impl Connection for HttpStream {
    fn connected(&self) -> Connected {
        let conn = Connected::new();
        let is_h2 = self
            .0
            .get_ref()
            .negotiated_alpn()
            .map(|alpn| alpn.as_slice() == b"h2")
            .unwrap_or_default();
        if is_h2 { conn.negotiated_h2() } else { conn }
    }
}
