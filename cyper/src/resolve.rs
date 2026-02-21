use std::{fmt::Debug, net::IpAddr, sync::Arc};

use futures_util::{FutureExt, Stream, StreamExt, future::LocalBoxFuture, stream::LocalBoxStream};
use http::Uri;
use send_wrapper::SendWrapper;

#[allow(async_fn_in_trait)]
/// Trait for customizing DNS resolution in cyper.
pub trait Resolve {
    /// The error type when DNS resolution failed.
    type Err: Into<crate::Error>;

    /// Performs DNS resolution on a [`Uri`] and return a [`Stream`] of
    /// [`IpAddr`].
    async fn resolve(&self, uri: &Uri) -> Result<impl Stream<Item = IpAddr> + '_, Self::Err>;
}

#[derive(Clone)]
pub(crate) struct ArcResolver(SendWrapper<Arc<dyn TypeErasedResolve>>);

impl ArcResolver {
    pub(crate) fn new<R: Resolve + 'static>(resolver: R) -> Self {
        Self(SendWrapper::new(Arc::new(resolver)))
    }

    pub(crate) async fn resolve<'a>(
        &'a self,
        uri: &'a Uri,
    ) -> Result<impl Stream<Item = IpAddr> + 'a, crate::Error> {
        self.0.type_erased_resolve(uri).await
    }
}

impl Debug for ArcResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_tuple("ArcResolver").finish()
    }
}

trait TypeErasedResolve {
    fn type_erased_resolve<'a>(
        &'a self,
        uri: &'a Uri,
    ) -> LocalBoxFuture<'a, Result<LocalBoxStream<'a, IpAddr>, crate::Error>>;
}

impl<T: Resolve> TypeErasedResolve for T {
    fn type_erased_resolve<'a>(
        &'a self,
        uri: &'a Uri,
    ) -> LocalBoxFuture<'a, Result<LocalBoxStream<'a, IpAddr>, crate::Error>> {
        async move {
            let addrs = self.resolve(uri).await.map_err(Into::into)?;
            Ok::<_, crate::Error>(addrs.boxed_local())
        }
        .boxed_local()
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use futures_util::stream;
    use http::StatusCode;

    use super::*;
    use crate::Client;

    struct TestResolver;

    impl Resolve for TestResolver {
        type Err = crate::Error;

        async fn resolve(&self, _uri: &Uri) -> Result<impl Stream<Item = IpAddr> + '_, Self::Err> {
            Ok(stream::iter([IpAddr::V4(Ipv4Addr::new(1, 0, 0, 1))]))
        }
    }

    #[compio::test]
    async fn test() {
        // why use cloudflare IP?
        // many well-known domain has a lot of IPs, they don't promise that IPs will not
        // change, except CloudFlare one.one.one.one, it always has 2 IPs 1.1.1.1 and
        // 1.0.0.1 at least, choose 1.0.0.1 instead of 1.1.1.1 can make most developers
        // can run this test without change, because in some places due to outdated
        // network settings, 1.1.1.1 cannot be accessed normally by developers.
        let client = Client::builder().custom_resolver(TestResolver).build();

        let resp = client
            .get("https://one.one.one.one")
            .unwrap()
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }
}
