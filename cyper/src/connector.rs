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
    inner: PlainConnector,
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
            inner: PlainConnector::new(tls, resolver),
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
                let proxy_uri = intercepted.uri().clone();
                let auth = intercepted.basic_auth().cloned();
                return Box::pin(SendWrapper::new(connect_via_proxy(
                    self.inner.clone(),
                    dst,
                    proxy_uri,
                    auth,
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
    connector: PlainConnector,
    dst: Uri,
    proxy_uri: Uri,
    auth: Option<http::HeaderValue>,
    proxy: Intercepted,
) -> crate::Result<HttpStream<HttpStream>> {
    if cfg!(feature = "socks")
        && matches!(
            proxy_uri.scheme_str(),
            Some("socks4" | "socks4a" | "socks5" | "socks5h")
        )
    {
        todo!()
    }
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
struct PlainConnector {
    tls: TlsBackend,
    resolver: Option<ArcResolver>,
}

impl PlainConnector {
    pub fn new(tls: TlsBackend, resolver: Option<ArcResolver>) -> Self {
        Self { tls, resolver }
    }
}

impl Service<Uri> for PlainConnector {
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
