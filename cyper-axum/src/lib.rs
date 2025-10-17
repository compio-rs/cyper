//! Adaptor for axum based on cyper.
//!
//! This crate just provides [`serve`] method, which is an in-place replacement
//! for `axum::serve`. See [`axum`] crate for its usage.

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
    time::{Duration, Instant},
};

use axum::{Router, handler::HandlerService, routing::MethodRouter};
use axum_core::{body::Body, extract::Request, response::Response};
use compio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream, UnixListener, UnixStream},
};
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

/// Types that can listen for connections.
pub trait Listener: 'static {
    /// The listener's IO type.
    type Io: AsyncRead + AsyncWrite + Unpin + 'static;

    /// The listener's address type.
    type Addr;

    /// Accept a new incoming connection to this listener.
    ///
    /// If the underlying accept call can return an error, this function must
    /// take care of logging and retrying.
    fn accept(&mut self) -> impl Future<Output = (Self::Io, Self::Addr)>;

    /// Returns the local address that this listener is bound to.
    fn local_addr(&self) -> io::Result<Self::Addr>;
}

impl Listener for TcpListener {
    type Addr = SocketAddr;
    type Io = TcpStream;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let mut instant = Instant::now();
        loop {
            match Self::accept(self).await {
                Ok(tup) => return tup,
                Err(e)
                    if !is_connection_error(&e) && instant.elapsed() >= Duration::from_secs(1) =>
                {
                    error!("accept error: {e}");
                    instant = Instant::now();
                }
                Err(_) => (),
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Self::local_addr(self)
    }
}

impl Listener for UnixListener {
    type Addr = socket2::SockAddr;
    type Io = UnixStream;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let mut instant = Instant::now();
        loop {
            match Self::accept(self).await {
                Ok(tup) => return tup,
                Err(e)
                    if !is_connection_error(&e) && instant.elapsed() >= Duration::from_secs(1) =>
                {
                    error!("accept error: {e}");
                    instant = Instant::now();
                }
                Err(_) => (),
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Self::local_addr(self)
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
pub fn serve<L, M, S>(listener: L, make_service: M) -> Serve<L, M, S>
where
    L: Listener,
    M: for<'a> Service<IncomingStream<'a, L>, Error = Infallible, Response = S>,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + 'static,
{
    Serve {
        listener,
        make_service,
        _marker: PhantomData,
    }
}

/// Future returned by [`serve`].
#[must_use = "futures must be awaited or polled"]
pub struct Serve<L, M, S> {
    listener: L,
    make_service: M,
    _marker: PhantomData<S>,
}

impl<L, M, S> Serve<L, M, S> {
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
    pub fn with_graceful_shutdown<F>(self, signal: F) -> WithGracefulShutdown<L, M, S, F>
    where
        F: Future<Output = ()> + 'static,
    {
        WithGracefulShutdown {
            listener: self.listener,
            make_service: self.make_service,
            signal,
            _marker: PhantomData,
        }
    }
}

impl<L, M, S> Serve<L, M, S>
where
    L: Listener,
{
    /// Returns the local address this server is bound to.
    pub fn local_addr(&self) -> io::Result<L::Addr> {
        self.listener.local_addr()
    }
}

impl<L, M, S> Debug for Serve<L, M, S>
where
    L: Debug + 'static,
    M: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            listener,
            make_service,
            _marker: _,
        } = self;

        f.debug_struct("Serve")
            .field("listener", listener)
            .field("make_service", make_service)
            .finish()
    }
}

impl<L, M, S> IntoFuture for Serve<L, M, S>
where
    L: Listener,
    M: for<'a> Service<IncomingStream<'a, L>, Error = Infallible, Response = S> + 'static,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + 'static,
{
    type IntoFuture = ServeFuture;
    type Output = io::Result<()>;

    fn into_future(self) -> Self::IntoFuture {
        ServeFuture(Box::pin(SendWrapper::new(async move {
            let Self {
                mut listener,
                mut make_service,
                _marker: _,
            } = self;

            loop {
                let (io, remote_addr) = listener.accept().await;
                let io = HyperStream::new(io);

                poll_fn(|cx| make_service.poll_ready(cx))
                    .await
                    .unwrap_or_else(|err| match err {});

                let tower_service = make_service
                    .call(IncomingStream {
                        io: &io,
                        remote_addr,
                    })
                    .await
                    .unwrap_or_else(|err| match err {})
                    .map_request(|req: Request<Incoming>| req.map(Body::new));
                let hyper_service = TowerToHyperService::new(tower_service);

                compio::runtime::spawn(async move {
                    #[allow(clippy::redundant_pattern_matching)]
                    if let Err(_) = Builder::new(CompioExecutor)
                        // upgrades needed for websockets
                        .serve_connection_with_upgrades(io, ServiceSendWrapper::new(hyper_service))
                        .await
                    {
                        // This error only appears when the client doesn't
                        // send a request and
                        // terminates the connection.
                        //
                        // Whenever the client sends one request
                        // then terminates the connection, it
                        // doesn't appear.
                    };
                })
                .detach();
            }
        })))
    }
}

/// Serve future with graceful shutdown enabled.
#[must_use = "futures must be awaited or polled"]
pub struct WithGracefulShutdown<L, M, S, F> {
    listener: L,
    make_service: M,
    signal: F,
    _marker: PhantomData<S>,
}

impl<L: Listener, M, S, F> WithGracefulShutdown<L, M, S, F> {
    /// Returns the local address this server is bound to.
    pub fn local_addr(&self) -> io::Result<L::Addr> {
        self.listener.local_addr()
    }
}

impl<L, M, S, F> Debug for WithGracefulShutdown<L, M, S, F>
where
    L: Debug + 'static,
    M: Debug,
    S: Debug,
    F: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            listener,
            make_service,
            signal,
            _marker: _,
        } = self;

        f.debug_struct("WithGracefulShutdown")
            .field("listener", listener)
            .field("make_service", make_service)
            .field("signal", signal)
            .finish()
    }
}

impl<L, M, S, F> IntoFuture for WithGracefulShutdown<L, M, S, F>
where
    L: Listener,
    L::Addr: Debug,
    M: for<'a> Service<IncomingStream<'a, L>, Error = Infallible, Response = S> + 'static,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + 'static,
    F: Future<Output = ()> + 'static,
{
    type IntoFuture = ServeFuture;
    type Output = io::Result<()>;

    fn into_future(self) -> Self::IntoFuture {
        let Self {
            mut listener,
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
                let (io, remote_addr) = futures_util::select_biased! {
                    _ = signal_tx.closed().fuse() => {
                        trace!("signal received, not accepting new connections");
                        break;
                    }
                    conn = listener.accept().fuse() => conn,
                };

                let io = HyperStream::new(io);

                trace!("connection {remote_addr:?} accepted");

                poll_fn(|cx| make_service.poll_ready(cx))
                    .await
                    .unwrap_or_else(|err| match err {});

                let tower_service = make_service
                    .call(IncomingStream {
                        io: &io,
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
                    let conn = builder
                        .serve_connection_with_upgrades(io, ServiceSendWrapper::new(hyper_service));
                    pin_mut!(conn);

                    let signal_closed = signal_tx.closed().fuse();
                    pin_mut!(signal_closed);

                    loop {
                        futures_util::select_biased! {
                            _ = &mut signal_closed => {
                                trace!("signal received in task, starting graceful shutdown");
                                conn.as_mut().graceful_shutdown();
                            }
                            result = conn.as_mut().fuse() => {
                                if let Err(_err) = result {
                                    trace!("failed to serve connection: {_err:#}");
                                }
                                break;
                            }
                        }
                    }

                    drop(close_rx);
                })
                .detach();
            }

            drop(close_rx);
            drop(listener);

            trace!(
                "waiting for {} task(s) to finish",
                close_tx.receiver_count()
            );
            close_tx.closed().await;
            Ok(())
        })))
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
pub struct IncomingStream<'a, L: Listener> {
    io: &'a HyperStream<L::Io>,
    remote_addr: L::Addr,
}

impl<L: Listener> IncomingStream<'_, L> {
    /// Get a reference to the inner IO type.
    pub fn io(&self) -> &L::Io {
        self.io.get_ref()
    }

    /// Returns the remote address that this stream is bound to.
    pub fn remote_addr(&self) -> &L::Addr {
        &self.remote_addr
    }
}

impl<L, H, T, S> Service<IncomingStream<'_, L>> for HandlerService<H, T, S>
where
    L: Listener,
    H: Clone,
    S: Clone,
{
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;
    type Response = Self;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: IncomingStream<'_, L>) -> Self::Future {
        std::future::ready(Ok(self.clone()))
    }
}

impl<L> Service<IncomingStream<'_, L>> for MethodRouter<()>
where
    L: Listener,
{
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;
    type Response = Self;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: IncomingStream<'_, L>) -> Self::Future {
        std::future::ready(Ok(self.clone().with_state(())))
    }
}

impl<L> Service<IncomingStream<'_, L>> for Router<()>
where
    L: Listener,
{
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;
    type Response = Self;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: IncomingStream<'_, L>) -> Self::Future {
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
