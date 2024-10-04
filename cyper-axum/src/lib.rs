//! Adaptor for axum based on cyper.

#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use std::{
    convert::Infallible,
    fmt::Debug,
    future::{Future, IntoFuture, poll_fn},
    io,
    marker::PhantomData,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::{Router, handler::HandlerService, routing::MethodRouter};
use axum_core::{body::Body, extract::Request, response::Response};
use compio::net::{TcpListener, TcpStream};
use compio_log::*;
use cyper_core::{CompioExecutor, HyperStream};
use futures_util::{FutureExt, pin_mut};
use hyper::body::Incoming;
use hyper_util::{server::conn::auto::Builder, service::TowerToHyperService};
use send_wrapper::SendWrapper;
// hyper crate also uses tokio channels. Use them here for consistency with axum.
use tokio::sync::watch;
use tower::ServiceExt as _;
use tower_service::Service;

/// Serve the service with the supplied listener.
///
/// This method of running a service is intentionally simple and doesn't support
/// any configuration. Use hyper or hyper-util if you need configuration.
///
/// It supports both HTTP/1 as well as HTTP/2.
///
/// # Examples
///
/// Serving a [`Router`]:
///
/// ```
/// use axum::{Router, routing::get};
///
/// # async {
/// let router = Router::new().route("/", get(|| async { "Hello, World!" }));
///
/// let listener = compio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
/// cyper_axum::serve(listener, router).await.unwrap();
/// # };
/// ```
///
/// See also [`Router::into_make_service_with_connect_info`].
///
/// Serving a [`MethodRouter`]:
///
/// ```
/// use axum::routing::get;
///
/// # async {
/// let router = get(|| async { "Hello, World!" });
///
/// let listener = compio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
/// cyper_axum::serve(listener, router).await.unwrap();
/// # };
/// ```
///
/// See also [`MethodRouter::into_make_service_with_connect_info`].
///
/// Serving a [`Handler`]:
///
/// ```
/// use axum::handler::HandlerWithoutStateExt;
///
/// # async {
/// async fn handler() -> &'static str {
///     "Hello, World!"
/// }
///
/// let listener = compio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
/// cyper_axum::serve(listener, handler.into_make_service())
///     .await
///     .unwrap();
/// # };
/// ```
///
/// See also [`HandlerWithoutStateExt::into_make_service_with_connect_info`] and
/// [`HandlerService::into_make_service_with_connect_info`].
///
/// [`Router`]: crate::Router
/// [`Router::into_make_service_with_connect_info`]: crate::Router::into_make_service_with_connect_info
/// [`MethodRouter`]: crate::routing::MethodRouter
/// [`MethodRouter::into_make_service_with_connect_info`]: crate::routing::MethodRouter::into_make_service_with_connect_info
/// [`Handler`]: crate::handler::Handler
/// [`HandlerWithoutStateExt::into_make_service_with_connect_info`]: crate::handler::HandlerWithoutStateExt::into_make_service_with_connect_info
/// [`HandlerService::into_make_service_with_connect_info`]: crate::handler::HandlerService::into_make_service_with_connect_info
pub fn serve<M, S>(tcp_listener: TcpListener, make_service: M) -> Serve<M, S>
where
    M: for<'a> Service<IncomingStream<'a>, Error = Infallible, Response = S>,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + 'static,
{
    Serve {
        tcp_listener,
        make_service,
        _marker: PhantomData,
    }
}

/// Future returned by [`serve`].
#[must_use = "futures must be awaited or polled"]
pub struct Serve<M, S> {
    tcp_listener: TcpListener,
    make_service: M,
    _marker: PhantomData<S>,
}

impl<M, S> Serve<M, S> {
    /// Prepares a server to handle graceful shutdown when the provided future
    /// completes.
    ///
    /// # Example
    ///
    /// ```
    /// use axum::{Router, routing::get};
    ///
    /// # async {
    /// let router = Router::new().route("/", get(|| async { "Hello, World!" }));
    ///
    /// let listener = compio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    /// cyper_axum::serve(listener, router)
    ///     .with_graceful_shutdown(shutdown_signal())
    ///     .await
    ///     .unwrap();
    /// # };
    ///
    /// async fn shutdown_signal() {
    ///     // ...
    /// }
    /// ```
    pub fn with_graceful_shutdown<F>(self, signal: F) -> WithGracefulShutdown<M, S, F>
    where
        F: Future<Output = ()> + 'static,
    {
        WithGracefulShutdown {
            tcp_listener: self.tcp_listener,
            make_service: self.make_service,
            signal,
            _marker: PhantomData,
        }
    }

    /// Returns the local address this server is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.tcp_listener.local_addr()
    }
}

impl<M, S> Debug for Serve<M, S>
where
    M: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            tcp_listener,
            make_service,
            _marker: _,
        } = self;

        f.debug_struct("Serve")
            .field("tcp_listener", tcp_listener)
            .field("make_service", make_service)
            .finish()
    }
}

impl<M, S> IntoFuture for Serve<M, S>
where
    M: for<'a> Service<IncomingStream<'a>, Error = Infallible, Response = S> + 'static,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + 'static,
{
    type IntoFuture = ServeFuture;
    type Output = io::Result<()>;

    fn into_future(self) -> Self::IntoFuture {
        ServeFuture(Box::pin(SendWrapper::new(async move {
            let Self {
                tcp_listener,
                mut make_service,
                _marker: _,
            } = self;

            loop {
                let (tcp_stream, remote_addr) = match tcp_accept(&tcp_listener).await {
                    Some(conn) => conn,
                    None => continue,
                };

                let tcp_stream = HyperStream::new(tcp_stream);

                poll_fn(|cx| make_service.poll_ready(cx))
                    .await
                    .unwrap_or_else(|err| match err {});

                let tower_service = make_service
                    .call(IncomingStream {
                        tcp_stream: &tcp_stream,
                        remote_addr,
                    })
                    .await
                    .unwrap_or_else(|err| match err {})
                    .map_request(|req: Request<Incoming>| req.map(Body::new));

                let hyper_service = TowerToHyperService::new(tower_service);

                compio::runtime::spawn(async move {
                    match Builder::new(CompioExecutor)
                        // upgrades needed for websockets
                        .serve_connection_with_upgrades(
                            tcp_stream,
                            ServiceSendWrapper::new(hyper_service),
                        )
                        .await
                    {
                        Ok(()) => {}
                        Err(_err) => {
                            // This error only appears when the client doesn't
                            // send a request and
                            // terminate the connection.
                            //
                            // If client sends one request then terminate
                            // connection whenever, it doesn't
                            // appear.
                        }
                    }
                })
                .detach();
            }
        })))
    }
}

/// Serve future with graceful shutdown enabled.
#[must_use = "futures must be awaited or polled"]
pub struct WithGracefulShutdown<M, S, F> {
    tcp_listener: TcpListener,
    make_service: M,
    signal: F,
    _marker: PhantomData<S>,
}

impl<M, S, F> WithGracefulShutdown<M, S, F> {
    /// Returns the local address this server is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.tcp_listener.local_addr()
    }
}

impl<M, S, F> Debug for WithGracefulShutdown<M, S, F>
where
    M: Debug,
    S: Debug,
    F: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            tcp_listener,
            make_service,
            signal,
            _marker: _,
        } = self;

        f.debug_struct("WithGracefulShutdown")
            .field("tcp_listener", tcp_listener)
            .field("make_service", make_service)
            .field("signal", signal)
            .finish()
    }
}

impl<M, S, F> IntoFuture for WithGracefulShutdown<M, S, F>
where
    M: for<'a> Service<IncomingStream<'a>, Error = Infallible, Response = S> + 'static,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + 'static,
    F: Future<Output = ()> + 'static,
{
    type IntoFuture = ServeFuture;
    type Output = io::Result<()>;

    fn into_future(self) -> Self::IntoFuture {
        let Self {
            tcp_listener,
            mut make_service,
            signal,
            _marker: _,
        } = self;

        let (signal_tx, signal_rx) = watch::channel(());
        let signal_tx = Arc::new(signal_tx);
        compio::runtime::spawn(async move {
            signal.await;
            trace!("received graceful shutdown signal. Telling tasks to shutdown");
            drop(signal_rx);
        })
        .detach();

        let (close_tx, close_rx) = watch::channel(());

        ServeFuture(Box::pin(SendWrapper::new(async move {
            loop {
                let (tcp_stream, remote_addr) = futures_util::select! {
                    conn = tcp_accept(&tcp_listener).fuse() => {
                        match conn {
                            Some(conn) => conn,
                            None => continue,
                        }
                    }
                    _ = signal_tx.closed().fuse() => {
                        trace!("signal received, not accepting new connections");
                        break;
                    }
                };

                let tcp_stream = HyperStream::new(tcp_stream);

                trace!("connection {remote_addr} accepted");

                poll_fn(|cx| make_service.poll_ready(cx))
                    .await
                    .unwrap_or_else(|err| match err {});

                let tower_service = make_service
                    .call(IncomingStream {
                        tcp_stream: &tcp_stream,
                        remote_addr,
                    })
                    .await
                    .unwrap_or_else(|err| match err {})
                    .map_request(|req: Request<Incoming>| req.map(Body::new));

                let hyper_service = TowerToHyperService::new(tower_service);

                let signal_tx = Arc::clone(&signal_tx);

                let close_rx = close_rx.clone();

                compio::runtime::spawn(async move {
                    let builder = Builder::new(CompioExecutor);
                    let conn = builder.serve_connection_with_upgrades(
                        tcp_stream,
                        ServiceSendWrapper::new(hyper_service),
                    );
                    pin_mut!(conn);

                    let signal_closed = signal_tx.closed().fuse();
                    pin_mut!(signal_closed);

                    loop {
                        futures_util::select! {
                            result = conn.as_mut().fuse() => {
                                if let Err(_err) = result {
                                    trace!("failed to serve connection: {_err:#}");
                                }
                                break;
                            }
                            _ = &mut signal_closed => {
                                trace!("signal received in task, starting graceful shutdown");
                                conn.as_mut().graceful_shutdown();
                            }
                        }
                    }

                    trace!("connection {remote_addr} closed");

                    drop(close_rx);
                })
                .detach();
            }

            drop(close_rx);
            drop(tcp_listener);

            trace!(
                "waiting for {} task(s) to finish",
                close_tx.receiver_count()
            );
            close_tx.closed().await;

            Ok(())
        })))
    }
}

fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

async fn tcp_accept(listener: &TcpListener) -> Option<(TcpStream, SocketAddr)> {
    match listener.accept().await {
        Ok(conn) => Some(conn),
        Err(e) => {
            if is_connection_error(&e) {
                return None;
            }

            // [From `hyper::Server` in 0.14](https://github.com/hyperium/hyper/blob/v0.14.27/src/server/tcp.rs#L186)
            //
            // > A possible scenario is that the process has hit the max open files
            // > allowed, and so trying to accept a new connection will fail with
            // > `EMFILE`. In some cases, it's preferable to just wait for some time, if
            // > the application will likely close some files (or connections), and try
            // > to accept the connection again. If this option is `true`, the error
            // > will be logged at the `error` level, since it is still a big deal,
            // > and then the listener will sleep for 1 second.
            //
            // hyper allowed customizing this but axum does not.
            error!("accept error: {e}");
            compio::time::sleep(Duration::from_secs(1)).await;
            None
        }
    }
}

#[doc(hidden)]
pub struct ServeFuture(futures_util::future::BoxFuture<'static, io::Result<()>>);

impl Future for ServeFuture {
    type Output = io::Result<()>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(cx)
    }
}

impl std::fmt::Debug for ServeFuture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeFuture").finish_non_exhaustive()
    }
}

/// An incoming stream.
///
/// Used with [`serve`] and [`IntoMakeServiceWithConnectInfo`].
///
/// [`IntoMakeServiceWithConnectInfo`]: crate::extract::connect_info::IntoMakeServiceWithConnectInfo
#[derive(Debug)]
pub struct IncomingStream<'a> {
    tcp_stream: &'a HyperStream<TcpStream>,
    remote_addr: SocketAddr,
}

impl IncomingStream<'_> {
    /// Returns the local address that this stream is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp_stream.get_ref().local_addr()
    }

    /// Returns the remote address that this stream is bound to.
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }
}

impl<H, T, S> Service<IncomingStream<'_>> for HandlerService<H, T, S>
where
    H: Clone,
    S: Clone,
{
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;
    type Response = Self;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: IncomingStream<'_>) -> Self::Future {
        std::future::ready(Ok(self.clone()))
    }
}

impl Service<IncomingStream<'_>> for MethodRouter<()> {
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;
    type Response = Self;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: IncomingStream<'_>) -> Self::Future {
        std::future::ready(Ok(self.clone().with_state(())))
    }
}

impl Service<IncomingStream<'_>> for Router<()> {
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;
    type Response = Self;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: IncomingStream<'_>) -> Self::Future {
        // call `Router::with_state` such that everything is turned into `Route` eagerly
        // rather than doing that per request
        std::future::ready(Ok(self.clone().with_state(())))
    }
}

struct ServiceSendWrapper<T>(SendWrapper<T>);

impl<T> ServiceSendWrapper<T> {
    pub fn new(v: T) -> Self {
        Self(SendWrapper::new(v))
    }
}

impl<R, T: hyper::service::Service<R>> hyper::service::Service<R> for ServiceSendWrapper<T> {
    type Error = T::Error;
    type Future = SendWrapper<T::Future>;
    type Response = T::Response;

    fn call(&self, req: R) -> Self::Future {
        SendWrapper::new(self.0.call(req))
    }
}
