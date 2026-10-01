use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll, ready},
};

use compio::{net::TcpStream, runtime::fd::PollFd, tls::TlsConnector};
use cyper_core::HyperStream;
use futures_util::{AsyncRead, AsyncWrite, StreamExt};
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use socket2::Socket;

use crate::{Error, Result, resolve::SharedResolver};

/// A HTTP stream wrapper, based on compio, and exposes [`hyper::rt`]
/// interfaces.
pub struct HttpStream<S = PollFd<Socket>> {
    inner: HyperStream<S>,
    is_proxy: bool,
    is_h2: bool,
}

impl HttpStream {
    /// Create [`HttpStream`] with target uri and TLS backend.
    pub async fn connect(
        uri: Uri,
        tls: Option<TlsConnector>,
        resolver: Option<SharedResolver>,
        is_proxy: bool,
    ) -> Result<Self> {
        let scheme = uri.scheme_str().unwrap_or("http");
        let host = uri.host().expect("there should be host");
        // `Uri::host()` includes brackets for IPv6, we must strip them.
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        let port = uri.port_u16();
        let stream = match scheme {
            "http" => {
                let port = port.unwrap_or(80);
                let stream = Self::connect_tcp(&uri, host, port, resolver).await?;
                // Ignore it.
                let _tls = tls;
                HyperStream::new_plain(stream.into_poll_fd()?)
            }
            #[cfg(tls)]
            "https" => {
                let port = port.unwrap_or(443);
                let stream = Self::connect_tcp(&uri, host, port, resolver).await?;
                let connector = tls.ok_or_else(|| Error::NoTlsBackend)?;
                HyperStream::new_tls(connector.connect(host, stream.into_poll_fd()?).await?)
            }
            _ => return Err(Error::BadScheme(scheme.to_string())),
        };
        let is_h2 = stream
            .negotiated_alpn()
            .map(|alpn| *alpn == *b"h2")
            .unwrap_or_default();
        Ok(Self {
            inner: stream,
            is_proxy,
            is_h2,
        })
    }

    async fn connect_tcp(
        uri: &Uri,
        host: &str,
        port: u16,
        resolver: Option<SharedResolver>,
    ) -> Result<TcpStream> {
        let stream = match resolver {
            None => TcpStream::connect((host, port)).await?,

            Some(resolver) => {
                let addrs = resolver
                    .resolve(uri)
                    .await?
                    .map(|ip| SocketAddr::new(ip, port))
                    .collect::<Vec<_>>()
                    .await;

                TcpStream::connect(addrs.as_slice()).await?
            }
        };

        Ok(stream)
    }

    pub fn into_wrapped(self) -> WrappedHttpStream {
        WrappedHttpStream::Plain(self)
    }
}

#[cfg(tls)]
impl HttpStream<HttpStream> {
    pub fn into_wrapped(self) -> WrappedHttpStream {
        WrappedHttpStream::Embedded(self)
    }
}

#[cfg(tls)]
impl<S: AsyncRead + AsyncWrite + Unpin> HttpStream<S> {
    pub async fn connect_with_https(
        stream: S,
        uri: Uri,
        tls: Option<TlsConnector>,
    ) -> Result<Self> {
        let host = uri.host().expect("there should be host");
        // `Uri::host()` includes brackets for IPv6, we must strip them.
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        let connector = tls.ok_or_else(|| Error::NoTlsBackend)?;
        let stream = HyperStream::new_tls(connector.connect(host, stream).await?);
        let is_h2 = stream
            .negotiated_alpn()
            .map(|alpn| *alpn == *b"h2")
            .unwrap_or_default();
        Ok(Self {
            inner: stream,
            is_proxy: false,
            is_h2,
        })
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> hyper::rt::Read for HttpStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        // Flush any buffered writes before reading. This is necessary
        // because code like hyper_util::rt::write_all (used by Tunnel
        // and SOCKS handshakes) and hyper's own body encoder may call
        // poll_write without poll_flush, leaving data buffered in
        // compio's AsyncWriteStream. Since HTTP/1.1 is half-duplex
        // (write then read), flushing here ensures the remote peer
        // receives our data before we wait for its response.
        // In HTTP/2 the stream is split, so this combined poll_read
        // is not called and concurrent reads/writes are unaffected.
        ready!(hyper::rt::Write::poll_flush(Pin::new(&mut self.inner), cx))?;
        hyper::rt::Read::poll_read(Pin::new(&mut self.inner), cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for HttpStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        futures_util::AsyncRead::poll_read(Pin::new(&mut self.inner), cx, buf)
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        futures_util::AsyncRead::poll_read_vectored(Pin::new(&mut self.inner), cx, bufs)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> hyper::rt::Write for HttpStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        hyper::rt::Write::poll_write(Pin::new(&mut self.inner), cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        hyper::rt::Write::poll_write_vectored(Pin::new(&mut self.inner), cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        hyper::rt::Write::poll_flush(Pin::new(&mut self.inner), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        hyper::rt::Write::poll_shutdown(Pin::new(&mut self.inner), cx)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for HttpStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        futures_util::AsyncWrite::poll_write(Pin::new(&mut self.inner), cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        futures_util::AsyncWrite::poll_write_vectored(Pin::new(&mut self.inner), cx, bufs)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        futures_util::AsyncWrite::poll_flush(Pin::new(&mut self.inner), cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        futures_util::AsyncWrite::poll_close(Pin::new(&mut self.inner), cx)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Connection for HttpStream<S> {
    fn connected(&self) -> Connected {
        let conn = Connected::new().proxy(self.is_proxy);
        if self.is_h2 {
            conn.negotiated_h2()
        } else {
            conn
        }
    }
}

#[allow(clippy::large_enum_variant)]
pub enum WrappedHttpStream {
    Plain(HttpStream),
    #[cfg(tls)]
    Embedded(HttpStream<HttpStream>),
}

impl hyper::rt::Read for WrappedHttpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            WrappedHttpStream::Plain(s) => hyper::rt::Read::poll_read(Pin::new(s), cx, buf),
            #[cfg(tls)]
            WrappedHttpStream::Embedded(s) => hyper::rt::Read::poll_read(Pin::new(s), cx, buf),
        }
    }
}

impl hyper::rt::Write for WrappedHttpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            WrappedHttpStream::Plain(s) => hyper::rt::Write::poll_write(Pin::new(s), cx, buf),
            #[cfg(tls)]
            WrappedHttpStream::Embedded(s) => hyper::rt::Write::poll_write(Pin::new(s), cx, buf),
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            WrappedHttpStream::Plain(s) => {
                hyper::rt::Write::poll_write_vectored(Pin::new(s), cx, bufs)
            }
            #[cfg(tls)]
            WrappedHttpStream::Embedded(s) => {
                hyper::rt::Write::poll_write_vectored(Pin::new(s), cx, bufs)
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            WrappedHttpStream::Plain(s) => s.is_write_vectored(),
            #[cfg(tls)]
            WrappedHttpStream::Embedded(s) => s.is_write_vectored(),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            WrappedHttpStream::Plain(s) => hyper::rt::Write::poll_flush(Pin::new(s), cx),
            #[cfg(tls)]
            WrappedHttpStream::Embedded(s) => hyper::rt::Write::poll_flush(Pin::new(s), cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            WrappedHttpStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(tls)]
            WrappedHttpStream::Embedded(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

impl Connection for WrappedHttpStream {
    fn connected(&self) -> Connected {
        match self {
            WrappedHttpStream::Plain(s) => s.connected(),
            #[cfg(tls)]
            WrappedHttpStream::Embedded(s) => s.connected(),
        }
    }
}
