use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use hyper::Uri;
use send_wrapper::SendWrapper;
use tower_service::Service;

use crate::{HttpStream, TlsBackend, proxy, resolve::ArcResolver};

/// An HTTP connector service.
///
/// It panics when called in a different thread other than the thread creates
/// it.
#[derive(Debug, Clone)]
pub struct Connector {
    tls: TlsBackend,
    resolver: Option<ArcResolver>,
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
            tls,
            resolver,
            proxies,
        }
    }
}

impl Service<Uri> for Connector {
    type Error = crate::Error;
    type Future = Pin<Box<dyn Future<Output = crate::Result<Self::Response>> + Send>>;
    type Response = HttpStream;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        for matcher in self.proxies.iter() {
            if let Some(intercepted) = matcher.intercept(&dst) {
                let proxy_uri = intercepted.uri().clone();
                let auth = intercepted.basic_auth().cloned();
                let clone = self.clone();
                return Box::pin(SendWrapper::new(async move {
                    clone.connect_via_proxy(dst, proxy_uri, auth.as_ref()).await
                }));
            }
        }

        Box::pin(SendWrapper::new(HttpStream::connect(
            dst,
            self.tls.clone(),
            self.resolver.clone(),
        )))
    }
}

impl Connector {
    async fn connect_via_proxy(
        self,
        dst: Uri,
        proxy_uri: Uri,
        auth: Option<&http::HeaderValue>,
    ) -> crate::Result<HttpStream> {
        match dst.scheme_str() {
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Some("https") => {
                HttpStream::connect_tunneled(proxy_uri, &dst, auth, self.tls, self.resolver).await
            }
            _ => {
                // HTTP forward proxy
                HttpStream::connect_proxy(proxy_uri, self.tls, self.resolver).await
            }
        }
    }
}
