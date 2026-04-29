use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll, ready},
};

use compio::{
    BufResult,
    buf::{IoBuf, IoBufMut, IoVectoredBuf},
    io::{AsyncRead, AsyncWrite, util::Splittable},
    net::TcpStream,
};
use cyper_core::HyperStream;
use futures_util::StreamExt;
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};

use crate::{Error, Result, TlsBackend, resolve::ArcResolver};

/// A HTTP stream wrapper, based on compio, and exposes [`hyper::rt`]
/// interfaces.
pub struct HttpStream<S = TcpStream>
where
    S: Splittable,
{
    inner: HyperStream<S>,
    is_proxy: bool,
    is_h2: bool,
}

impl HttpStream {
    /// Create [`HttpStream`] with target uri and TLS backend.
    pub async fn connect(
        uri: Uri,
        tls: TlsBackend,
        resolver: Option<ArcResolver>,
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
                HyperStream::new_plain(stream)
            }
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            "https" => {
                let port = port.unwrap_or(443);
                let stream = Self::connect_tcp(&uri, host, port, resolver).await?;
                let connector = tls.create_connector()?;
                HyperStream::new_tls(connector.connect(host, stream).await?)
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
        resolver: Option<ArcResolver>,
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
}

impl<S: Splittable + 'static> HttpStream<S>
where
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
{
    pub async fn connect_with(stream: S, uri: Uri, tls: TlsBackend) -> Result<Self> {
        let scheme = uri.scheme_str().unwrap_or("http");
        let stream = match scheme {
            "http" => {
                let _tls = tls;
                HyperStream::new_plain(stream)
            }
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            "https" => {
                let host = uri.host().expect("there should be host");
                // `Uri::host()` includes brackets for IPv6, we must strip them.
                let host = host
                    .strip_prefix('[')
                    .and_then(|h| h.strip_suffix(']'))
                    .unwrap_or(host);
                let connector = tls.create_connector()?;
                HyperStream::new_tls(connector.connect(host, stream).await?)
            }
            _ => return Err(Error::BadScheme(scheme.to_string())),
        };
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

    pub fn into_wrapped(self) -> HttpStream<Self> {
        HttpStream {
            is_proxy: self.is_proxy,
            is_h2: self.is_h2,
            inner: HyperStream::new_plain(self),
        }
    }
}

impl<S: Splittable + 'static> hyper::rt::Read for HttpStream<S>
where
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
{
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
        ready!(hyper::rt::Write::poll_flush(
            Pin::new(&mut self.inner),
            cx
        ))?;
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: Splittable + 'static> hyper::rt::Write for HttpStream<S>
where
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
{
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

impl<S: Splittable + 'static> Connection for HttpStream<S>
where
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
{
    fn connected(&self) -> Connected {
        let conn = Connected::new().proxy(self.is_proxy);
        if self.is_h2 {
            conn.negotiated_h2()
        } else {
            conn
        }
    }
}

impl<S: Splittable + 'static> Splittable for HttpStream<S>
where
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
{
    type ReadHalf = HttpStreamReadHalf<S>;
    type WriteHalf = HttpStreamWriteHalf<S>;

    fn split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        let (read, write) = futures_util::AsyncReadExt::split(self.inner);
        (HttpStreamReadHalf(read), HttpStreamWriteHalf(write))
    }
}

pub struct HttpStreamReadHalf<S: Splittable>(futures_util::io::ReadHalf<HyperStream<S>>);

impl<S: Splittable + 'static> AsyncRead for HttpStreamReadHalf<S>
where
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
{
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        let res = futures_util::AsyncReadExt::read(&mut self.0, buf.ensure_init()).await;
        if let Ok(len) = &res {
            unsafe { buf.set_len(*len) };
        }
        BufResult(res, buf)
    }
}

pub struct HttpStreamWriteHalf<S: Splittable>(futures_util::io::WriteHalf<HyperStream<S>>);

impl<S: Splittable + 'static> AsyncWrite for HttpStreamWriteHalf<S>
where
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
{
    async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        let slice = buf.as_init();
        let res = futures_util::AsyncWriteExt::write(&mut self.0, slice).await;
        BufResult(res, buf)
    }

    async fn write_vectored<T: IoVectoredBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        let slices = buf.iter_slice().map(io::IoSlice::new).collect::<Vec<_>>();
        let res = futures_util::AsyncWriteExt::write_vectored(&mut self.0, &slices).await;
        BufResult(res, buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        futures_util::AsyncWriteExt::flush(&mut self.0).await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        futures_util::AsyncWriteExt::close(&mut self.0).await
    }
}
