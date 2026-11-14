use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::{Pin, pin},
    task::{Context, Poll, ready},
    time::Duration,
};

use compio::{
    net::{TcpStream, ToSocketAddrsAsync},
    runtime::fd::PollFd,
    tls::TlsConnector,
};
use cyper_core::HyperStream;
use futures_util::{
    AsyncRead, AsyncWrite, StreamExt,
    future::{Either, select},
    stream::FuturesUnordered,
};
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use socket2::Socket;

use crate::{Error, Result, resolve::SharedResolver};

/// How long a connection attempt may stay pending before the next address is
/// tried in parallel ("Connection Attempt Delay" of RFC 8305; hyper-util uses
/// the same default).
const CONNECTION_ATTEMPT_DELAY: Duration = Duration::from_millis(300);

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
        let addrs: Vec<SocketAddr> = match resolver {
            None => (host, port).to_socket_addrs_async().await?.collect(),
            Some(resolver) => {
                resolver
                    .resolve(uri)
                    .await?
                    .map(|ip| SocketAddr::new(ip, port))
                    .collect()
                    .await
            }
        };

        Ok(connect_staggered(addrs, CONNECTION_ATTEMPT_DELAY, TcpStream::connect).await?)
    }

    pub fn into_wrapped(self) -> WrappedHttpStream {
        WrappedHttpStream::Plain(self)
    }
}

/// Connects to the first address that accepts, racing attempts as in RFC 8305
/// ("Happy Eyeballs"): addresses alternate between the two families, and the
/// next attempt starts once the previous one has failed or `delay` has passed,
/// so a stalled address can't hold up the others. Losing attempts are dropped.
async fn connect_staggered<T, F, Fut>(
    addrs: Vec<SocketAddr>,
    delay: Duration,
    connect: F,
) -> io::Result<T>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    let mut addrs = interleave_families(addrs).into_iter();
    let mut attempts = FuturesUnordered::new();
    let mut last_err = None;
    loop {
        if let Some(addr) = addrs.next() {
            attempts.push(connect(addr));
        }
        if attempts.is_empty() {
            return Err(last_err.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "could not resolve to any addresses",
                )
            }));
        }
        let finished = if addrs.as_slice().is_empty() {
            attempts.next().await
        } else {
            match select(attempts.next(), pin!(compio::time::sleep(delay))).await {
                Either::Left((finished, _)) => finished,
                // Leave the pending attempts running and start the next one.
                Either::Right(_) => continue,
            }
        };
        match finished {
            Some(Ok(stream)) => return Ok(stream),
            Some(Err(err)) => last_err = Some(err),
            None => unreachable!("`attempts` is not empty"),
        }
    }
}

/// Reorders `addrs` to alternate between IPv6 and IPv4, starting with the
/// family of the first address and keeping the order within each family.
fn interleave_families(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let Some(first_is_ipv6) = addrs.first().map(SocketAddr::is_ipv6) else {
        return addrs;
    };
    let (preferred, other): (Vec<_>, Vec<_>) = addrs
        .into_iter()
        .partition(|addr| addr.is_ipv6() == first_is_ipv6);
    let mut other = other.into_iter();
    let mut interleaved = Vec::with_capacity(preferred.len() + other.len());
    for addr in preferred {
        interleaved.push(addr);
        interleaved.extend(other.next());
    }
    interleaved.extend(other);
    interleaved
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

#[cfg(test)]
mod tests {
    use std::{future::pending, time::Instant};

    use super::*;

    fn addr(addr: &str) -> SocketAddr {
        addr.parse().unwrap()
    }

    fn refused(addr: SocketAddr) -> io::Error {
        io::Error::new(io::ErrorKind::ConnectionRefused, addr.to_string())
    }

    #[test]
    fn interleave_starts_with_the_first_family() {
        let (v6a, v6b) = (addr("[2001:db8::1]:443"), addr("[2001:db8::2]:443"));
        let (v4a, v4b) = (addr("192.0.2.1:443"), addr("192.0.2.2:443"));
        assert_eq!(
            interleave_families(vec![v6a, v6b, v4a, v4b]),
            [v6a, v4a, v6b, v4b]
        );
        assert_eq!(interleave_families(vec![v4a, v6a, v6b]), [v4a, v6a, v6b]);
    }

    #[compio::test]
    async fn stalled_address_does_not_block_the_next() {
        let (stalled, reachable) = (addr("[2001:db8::1]:443"), addr("192.0.2.1:443"));
        let delay = Duration::from_millis(50);
        let start = Instant::now();
        let connected = connect_staggered(vec![stalled, reachable], delay, |addr| async move {
            if addr == stalled {
                pending().await
            } else {
                Ok(addr)
            }
        })
        .await
        .unwrap();
        assert_eq!(connected, reachable);
        assert!(start.elapsed() >= delay);
    }

    #[compio::test]
    async fn failed_attempt_starts_the_next_without_waiting() {
        let (failing, reachable) = (addr("[2001:db8::1]:443"), addr("192.0.2.1:443"));
        let connect = connect_staggered(
            vec![failing, reachable],
            Duration::from_secs(3600),
            |addr| async move {
                if addr == failing {
                    Err(refused(addr))
                } else {
                    Ok(addr)
                }
            },
        );
        let connected = compio::time::timeout(Duration::from_secs(5), connect)
            .await
            .expect("the next attempt waited for the delay")
            .unwrap();
        assert_eq!(connected, reachable);
    }

    #[compio::test]
    async fn every_address_failing_reports_the_last_error() {
        let (first, last) = (addr("[2001:db8::1]:443"), addr("192.0.2.1:443"));
        let err = connect_staggered(
            vec![first, last],
            Duration::from_millis(10),
            |addr| async move { Err::<(), _>(refused(addr)) },
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), last.to_string());

        let err = connect_staggered(Vec::new(), Duration::from_millis(10), |_| async {
            Ok::<(), io::Error>(())
        })
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
