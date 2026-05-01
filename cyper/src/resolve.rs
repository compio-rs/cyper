use std::{fmt::Debug, net::IpAddr};

use futures_util::{FutureExt, Stream, StreamExt, future::LocalBoxFuture, stream::LocalBoxStream};
use http::Uri;
use send_wrapper::SendWrapper;

use crate::sync::shared::Shared;

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
pub(crate) struct SharedResolver(SendWrapper<Shared<dyn TypeErasedResolve>>);

impl SharedResolver {
    pub(crate) fn new<R: Resolve + 'static>(resolver: R) -> Self {
        Self(SendWrapper::new(Shared::new(resolver)))
    }

    pub(crate) async fn resolve<'a>(
        &'a self,
        uri: &'a Uri,
    ) -> Result<impl Stream<Item = IpAddr> + 'a, crate::Error> {
        self.0.type_erased_resolve(uri).await
    }
}

impl Debug for SharedResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_tuple("SharedResolver").finish()
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
