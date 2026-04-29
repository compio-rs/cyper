use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};

use hyper::Uri;
use send_wrapper::SendWrapper;
use tower_service::Service;

use crate::{
    HttpStream, TlsBackend,
    proxy::{self, Intercepted},
    resolve::ArcResolver,
};

/// An HTTP connector service.
///
/// It panics when called in a different thread other than the thread creates
/// it.
#[derive(Debug, Clone)]
pub struct Connector {
    inner: HttpConnector,
    proxies: Arc<Vec<proxy::Matcher>>,
}

impl Connector {
    /// Creates the connector with specific TLS backend.
    pub fn new(
        tls: TlsBackend,
        resolver: Option<ArcResolver>,
        proxies: Arc<Vec<proxy::Matcher>>,
    ) -> Self {
        Self {
            inner: HttpConnector::new(tls, resolver),
            proxies,
        }
    }
}

impl Service<Uri> for Connector {
    type Error = crate::Error;
    type Future = Pin<Box<dyn Future<Output = crate::Result<Self::Response>> + Send>>;
    type Response = HttpStream<HttpStream>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        for matcher in self.proxies.iter() {
            if let Some(intercepted) = matcher.intercept(&dst) {
                return Box::pin(SendWrapper::new(connect_via_proxy(
                    self.inner.clone(),
                    dst,
                    intercepted,
                )));
            }
        }

        let fut = self.inner.call(dst);
        Box::pin(async {
            let stream = fut.await?;
            Ok(stream.into_wrapped())
        })
    }
}

/// A stream wrapper that flushes buffered writes before reading.
///
/// This is a **temporary** wrapper used during proxy CONNECT / SOCKS
/// handshakes. `hyper_util::rt::write_all` only calls `poll_write` (never
/// `poll_flush`), which would leave data buffered in compio's
/// `AsyncWriteStream` forever — causing a hang. Since the handshake
/// protocol is always write-then-read (CONNECT request → response,
/// SOCKS greeting → reply → request → reply), we flush any pending
/// buffered writes in `poll_read` so the remote peer actually receives
/// the data before we wait for its response.
///
/// The wrapper is stripped after the handshake via [`into_inner`],
/// preserving the `HttpStream<HttpStream>` nesting for
/// HTTPS-over-HTTPS-proxy support.
///
/// [`into_inner`]: AutoFlushWrite::into_inner
struct AutoFlushWrite<S>(S);

impl<S> AutoFlushWrite<S> {
    /// Strip the auto-flush wrapper, returning the underlying stream.
    fn into_inner(self) -> S {
        self.0
    }
}

impl<S> hyper::rt::Read for AutoFlushWrite<S>
where
    S: hyper::rt::Read + hyper::rt::Write + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        // Flush any buffered writes before reading. hyper_util's
        // write_all (used by Tunnel / SOCKS handshake) never calls
        // poll_flush; data would otherwise sit in compio's
        // AsyncWriteStream buffer forever while we wait for a
        // response that will never arrive.
        ready!(Pin::new(&mut self.0).poll_flush(cx))?;
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S> hyper::rt::Write for AutoFlushWrite<S>
where
    S: hyper::rt::Write + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// A connector adapter that wraps response streams in [`AutoFlushWrite`].
///
/// Used with `hyper_util`'s `Tunnel` and `SocksV4`/`SocksV5` so that
/// handshake writes are flushed immediately. The `AutoFlushWrite` wrapper
/// is stripped after the handshake — it never leaks into normal HTTP
/// request/response processing.
struct AutoFlushConnector<C> {
    inner: C,
}

impl<C> Service<Uri> for AutoFlushConnector<C>
where
    C: Service<Uri>,
    C::Future: Send + 'static,
    C::Response: Send + 'static,
    C::Error: Send + 'static,
{
    type Error = C::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    type Response = AutoFlushWrite<C::Response>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let fut = self.inner.call(dst);
        Box::pin(async move {
            let stream = fut.await?;
            Ok(AutoFlushWrite(stream))
        })
    }
}

async fn connect_via_proxy(
    connector: HttpConnector,
    dst: Uri,
    intercepted: Intercepted,
) -> crate::Result<HttpStream<HttpStream>> {
    let proxy_uri = intercepted.uri().clone();

    #[cfg(feature = "socks")]
    if matches!(
        proxy_uri.scheme_str(),
        Some("socks4" | "socks4a" | "socks5" | "socks5h")
    ) {
        // Wrap in AutoFlushConnector for the same reason as the Tunnel
        // path above: hyper_util's SOCKS handshake code uses write_all
        // without poll_flush, leaving data buffered in compio's
        // AsyncWriteStream.
        let tls = connector.tls.clone();
        return socks::connect(
            AutoFlushConnector { inner: connector },
            dst,
            intercepted,
            tls,
        )
        .await;
    }

    let auth = intercepted.basic_auth().cloned();

    match dst.scheme_str() {
        #[cfg(any(feature = "native-tls", feature = "rustls"))]
        Some("https") => {
            use hyper_util::client::legacy::connect::proxy::Tunnel;

            let tls = connector.tls.clone();
            // Wrap in AutoFlushConnector so the CONNECT request is flushed
            // to the proxy. hyper_util's write_all only calls poll_write
            // (never poll_flush), which would leave data buffered in
            // compio's AsyncWriteStream forever. AutoFlushWrite strips off
            // after the tunnel handshake (see connect_with below).
            let autoflush = AutoFlushConnector { inner: connector };
            let mut tunnel = Tunnel::new(proxy_uri, autoflush);
            if let Some(auth) = auth {
                tunnel = tunnel.with_auth(auth);
            }
            let tunneled = tunnel
                .call(dst.clone())
                .await
                .map_err(|e| crate::Error::Proxy(e.into()))?;
            // Strip the AutoFlushWrite wrapper — we only needed it for
            // the CONNECT handshake. Normal HTTP I/O through the tunnel
            // is handled by hyper's own protocol code which flushes
            // properly.
            HttpStream::connect_with(tunneled.into_inner(), dst, tls).await
        }
        _ => Ok(
            HttpStream::connect(proxy_uri, connector.tls, connector.resolver, true)
                .await?
                .into_wrapped(),
        ),
    }
}

#[derive(Debug, Clone)]
struct HttpConnector {
    tls: TlsBackend,
    resolver: Option<ArcResolver>,
}

impl HttpConnector {
    pub fn new(tls: TlsBackend, resolver: Option<ArcResolver>) -> Self {
        Self { tls, resolver }
    }
}

impl Service<Uri> for HttpConnector {
    type Error = crate::Error;
    type Future = Pin<Box<dyn Future<Output = crate::Result<Self::Response>> + Send>>;
    type Response = HttpStream;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let tls = self.tls.clone();
        let resolver = self.resolver.clone();
        Box::pin(SendWrapper::new(HttpStream::connect(
            dst, tls, resolver, false,
        )))
    }
}

#[cfg(feature = "socks")]
mod socks {
    use http::Uri;
    use hyper_util::client::legacy::connect::proxy::{SocksV4, SocksV5};
    use tower_service::Service;

    use super::{AutoFlushConnector, HttpConnector};
    use crate::{Error, HttpStream, TlsBackend, proxy::Intercepted};

    pub(super) async fn connect(
        connector: AutoFlushConnector<HttpConnector>,
        dst: Uri,
        intercepted: Intercepted,
        tls: TlsBackend,
    ) -> crate::Result<HttpStream<HttpStream>> {
        let proxy_uri = intercepted.uri().clone();
        let raw_auth = intercepted
            .raw_auth()
            .map(|(u, p)| (u.to_owned(), p.to_owned()));

        // Build an http:// URI for the HttpConnector to connect to the
        // SOCKS proxy via TCP. The SOCKS scheme (socks5://, etc.) only
        // indicates the handshake protocol, not the transport.
        let host = proxy_uri.host().expect("SOCKS proxy URI should have host");
        let port = proxy_uri.port_u16().unwrap_or(1080);
        let http_proxy_uri: Uri = format!("http://{host}:{port}")
            .parse()
            .expect("should be valid URI");

        let is_local_dns = matches!(proxy_uri.scheme_str(), Some("socks4") | Some("socks5"));

        // The SocksV4/V5 handshake uses write_all which doesn't flush.
        // AutoFlushConnector ensured every write was flushed; strip the
        // AutoFlushWrite wrapper now that the handshake is done.
        let stream: HttpStream = match proxy_uri.scheme_str() {
            Some("socks4") | Some("socks4a") => {
                let mut svc = SocksV4::new(http_proxy_uri, connector).local_dns(is_local_dns);
                svc.call(dst.clone())
                    .await
                    .map_err(|e| Error::Proxy(Box::new(e)))?
                    .into_inner()
            }
            Some("socks5") | Some("socks5h") => {
                let mut svc = SocksV5::new(http_proxy_uri, connector).local_dns(is_local_dns);
                if let Some((user, pass)) = raw_auth {
                    svc = svc.with_auth(user, pass);
                }
                svc.call(dst.clone())
                    .await
                    .map_err(|e| Error::Proxy(Box::new(e)))?
                    .into_inner()
            }
            _ => unreachable!(),
        };

        // After the SOCKS handshake we have a TCP tunnel to the destination.
        // Wrap with TLS if targeting HTTPS.
        match dst.scheme_str() {
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Some("https") => HttpStream::connect_with(stream, dst, tls).await,
            _ => Ok(stream.into_wrapped()),
        }
    }
}
