use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
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
        return socks::connect(connector, dst, intercepted).await;
    }

    let auth = intercepted.basic_auth().cloned();

    match dst.scheme_str() {
        #[cfg(any(feature = "native-tls", feature = "rustls"))]
        Some("https") => {
            use hyper_util::client::legacy::connect::proxy::Tunnel;

            let tls = connector.tls.clone();
            let mut tunnel = Tunnel::new(proxy_uri, connector);
            if let Some(auth) = auth {
                tunnel = tunnel.with_auth(auth);
            }
            let tunneled = tunnel
                .call(dst.clone())
                .await
                .map_err(|e| crate::Error::Proxy(e.into()))?;
            HttpStream::connect_with(tunneled, dst, tls).await
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

    use super::HttpConnector;
    use crate::{Error, HttpStream, proxy::Intercepted};

    pub(super) async fn connect(
        connector: HttpConnector,
        dst: Uri,
        intercepted: Intercepted,
    ) -> crate::Result<HttpStream<HttpStream>> {
        let proxy_uri = intercepted.uri().clone();
        let raw_auth = intercepted
            .raw_auth()
            .map(|(u, p)| (u.to_owned(), p.to_owned()));
        let tls = connector.tls.clone();

        // Build an http:// URI for the HttpConnector to connect to the
        // SOCKS proxy via TCP. The SOCKS scheme (socks5://, etc.) only
        // indicates the handshake protocol, not the transport.
        let host = proxy_uri.host().expect("SOCKS proxy URI should have host");
        let port = proxy_uri.port_u16().unwrap_or(1080);
        let http_proxy_uri: Uri = format!("http://{host}:{port}")
            .parse()
            .expect("should be valid URI");

        let is_local_dns = matches!(proxy_uri.scheme_str(), Some("socks4") | Some("socks5"));

        let stream: HttpStream = match proxy_uri.scheme_str() {
            Some("socks4") | Some("socks4a") => {
                let mut svc = SocksV4::new(http_proxy_uri, connector).local_dns(is_local_dns);
                svc.call(dst.clone())
                    .await
                    .map_err(|e| Error::Proxy(Box::new(e)))?
            }
            Some("socks5") | Some("socks5h") => {
                let mut svc = SocksV5::new(http_proxy_uri, connector).local_dns(is_local_dns);
                if let Some((user, pass)) = raw_auth {
                    svc = svc.with_auth(user, pass);
                }
                svc.call(dst.clone())
                    .await
                    .map_err(|e| Error::Proxy(Box::new(e)))?
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
