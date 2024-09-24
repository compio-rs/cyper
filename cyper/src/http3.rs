use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    future::Future,
    net::SocketAddr,
    sync::{
        mpsc::{Receiver, TryRecvError},
        Arc, Mutex,
    },
    time::Instant,
};

use compio::{
    buf::bytes::Bytes,
    net::ToSocketAddrsAsync,
    quic::{
        h3::{client::SendRequest, OpenStreams},
        ClientBuilder, ConnectError, Connecting, Connection, Endpoint,
    },
};
use http::{
    uri::{Authority, Scheme},
    Request, Uri,
};
use hyper::body::Buf;
use url::Url;

use crate::{Body, Error, Response, Result};

#[derive(Debug, Clone)]
struct DualEndpoint {
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "watchos",
        target_os = "tvos"
    )))]
    v4end: Endpoint,
    v6end: Endpoint,
}

impl DualEndpoint {
    fn client_builder() -> ClientBuilder<compio::rustls::ClientConfig> {
        ClientBuilder::new_with_no_server_verification()
            .with_key_log()
            .with_alpn_protocols(&["h3"])
    }

    async fn new() -> Result<Self> {
        Ok(Self {
            #[cfg(not(any(
                target_os = "linux",
                target_os = "macos",
                target_os = "ios",
                target_os = "watchos",
                target_os = "tvos"
            )))]
            v4end: Self::client_builder()
                .bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
                .await?,
            v6end: Self::client_builder()
                .bind((std::net::Ipv6Addr::UNSPECIFIED, 0))
                .await?,
        })
    }

    fn end(&self, is_v4: bool) -> &Endpoint {
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "watchos",
            target_os = "tvos"
        )))]
        {
            if is_v4 { &self.v4end } else { &self.v6end }
        }
        #[cfg(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "watchos",
            target_os = "tvos"
        ))]
        {
            &self.v6end
        }
    }

    fn connect(
        &self,
        remote: SocketAddr,
        server_name: &str,
    ) -> std::result::Result<Connecting, ConnectError> {
        self.end(remote.is_ipv4())
            .connect(remote, server_name, None)
    }
}

#[derive(Debug, Clone)]
struct Connector {
    endpoint: DualEndpoint,
}

impl Connector {
    pub async fn new() -> Result<Self> {
        Ok(Self {
            endpoint: DualEndpoint::new().await?,
        })
    }

    pub async fn connect(
        &self,
        dest: Uri,
    ) -> Result<(
        h3::client::Connection<Connection, Bytes>,
        SendRequest<OpenStreams, Bytes>,
    )> {
        let host = dest.host().expect("there should be host");
        let server_name = host.trim_start_matches('[').trim_end_matches(']');
        let port = dest.port_u16().unwrap_or(443);

        let mut err = None;
        for remote in (host, port).to_socket_addrs_async().await? {
            match async { Result::Ok(self.endpoint.connect(remote, server_name)?.await?) }.await {
                Ok(conn) => return Ok(compio::quic::h3::client::new(conn).await?),
                Err(e) => err = Some(e),
            }
        }
        Err(err.unwrap_or_else(|| {
            Error::H3Client("failed to establish connection for HTTP/3 request".into())
        }))
    }
}

#[derive(Clone)]
pub struct PoolClient {
    inner: SendRequest<OpenStreams, Bytes>,
}

impl PoolClient {
    pub fn new(tx: SendRequest<OpenStreams, Bytes>) -> Self {
        Self { inner: tx }
    }

    pub async fn send_request(&mut self, req: Request<Body>, url: Url) -> Result<Response> {
        use hyper::body::Body as _;

        let (head, req_body) = req.into_parts();
        let mut req = Request::from_parts(head, ());

        if let Some(n) = req_body.size_hint().exact() {
            if n > 0 {
                req.headers_mut()
                    .insert(http::header::CONTENT_LENGTH, n.into());
            }
        }

        let mut stream = self.inner.send_request(req).await?;

        let req_body = req_body.0;
        if let Some(b) = req_body {
            if !b.is_empty() {
                stream.send_data(b).await?;
            }
        }

        stream.finish().await?;

        let resp = stream.recv_response().await?;

        let mut resp_body = Vec::<u8>::new();
        while let Some(chunk) = stream.recv_data().await? {
            resp_body.extend(chunk.chunk())
        }

        Ok(Response::with_body(resp, Bytes::from(resp_body), url))
    }
}

impl Debug for PoolClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolClient").finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct PoolConnection {
    // This receives errors from polling h3 driver.
    close_rx: Receiver<h3::Error>,
    client: PoolClient,
    idle_timeout: Instant,
}

impl PoolConnection {
    pub fn new(client: PoolClient, close_rx: Receiver<h3::Error>) -> Self {
        Self {
            close_rx,
            client,
            idle_timeout: Instant::now(),
        }
    }

    pub fn pool(&mut self) -> PoolClient {
        self.idle_timeout = Instant::now();
        self.client.clone()
    }

    pub fn is_invalid(&self) -> bool {
        match self.close_rx.try_recv() {
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => true,
            Ok(_) => true,
        }
    }
}

type Key = (Scheme, Authority);

#[derive(Debug)]
struct PoolInner {
    connecting: HashSet<Key>,
    idle_conns: HashMap<Key, PoolConnection>,
}

impl PoolInner {
    fn insert(&mut self, key: Key, conn: PoolConnection) {
        self.idle_conns.insert(key, conn);
    }
}

#[derive(Debug, Clone)]
struct Pool {
    inner: Arc<Mutex<PoolInner>>,
}

impl Pool {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(PoolInner {
                connecting: HashSet::new(),
                idle_conns: HashMap::new(),
            })),
        }
    }

    pub fn connecting(&self, key: Key) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.connecting.insert(key.clone()) {
            return Err(Error::H3Client(format!(
                "HTTP/3 connecting already in progress for {key:?}"
            )));
        }
        Ok(())
    }

    pub fn try_pool(&self, key: &Key) -> Option<PoolClient> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(conn) = inner.idle_conns.get(key) {
            // We check first if the connection still valid
            // and if not, we remove it from the pool.
            if conn.is_invalid() {
                inner.idle_conns.remove(key);
                return None;
            }
        }

        inner.idle_conns.get_mut(key).map(|conn| conn.pool())
    }

    pub fn new_connection(
        &mut self,
        key: Key,
        mut driver: h3::client::Connection<Connection, Bytes>,
        tx: SendRequest<OpenStreams, Bytes>,
    ) -> PoolClient {
        let (close_tx, close_rx) = std::sync::mpsc::channel();
        compio::runtime::spawn(async move {
            if let Err(e) = driver.wait_idle().await {
                close_tx.send(e).ok();
            }
        })
        .detach();

        let mut inner = self.inner.lock().unwrap();

        let client = PoolClient::new(tx);
        let conn = PoolConnection::new(client.clone(), close_rx);
        inner.insert(key.clone(), conn);

        // We clean up "connecting" here so we don't have to acquire the lock again.
        let existed = inner.connecting.remove(&key);
        debug_assert!(existed, "key not in connecting set");

        client
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    pool: Pool,
    connector: Connector,
}

impl Client {
    pub async fn new() -> Result<Self> {
        Ok(Self {
            pool: Pool::new(),
            connector: Connector::new().await?,
        })
    }

    async fn get_pooled_client(&mut self, key: Key) -> Result<PoolClient> {
        if let Some(client) = self.pool.try_pool(&key) {
            return Ok(client);
        }

        let dest = domain_as_uri(key.clone());
        self.pool.connecting(key.clone())?;
        let (driver, tx) = self.connector.connect(dest).await?;
        Ok(self.pool.new_connection(key, driver, tx))
    }

    async fn send_request(mut self, key: Key, req: Request<Body>, url: Url) -> Result<Response> {
        let mut pooled = self.get_pooled_client(key).await?;
        pooled.send_request(req, url).await
    }

    pub async fn request(&self, mut req: Request<Body>, url: &Url) -> Result<Response> {
        let pool_key = extract_domain(req.uri_mut())?;
        self.clone().send_request(pool_key, req, url.clone()).await
    }
}

fn extract_domain(uri: &mut Uri) -> Result<Key> {
    let uri_clone = uri.clone();
    match (uri_clone.scheme(), uri_clone.authority()) {
        (Some(scheme), Some(auth)) => Ok((scheme.clone(), auth.clone())),
        _ => Err(Error::H3Client("failed to extract domain".into())),
    }
}

fn domain_as_uri((scheme, auth): Key) -> Uri {
    http::uri::Builder::new()
        .scheme(scheme)
        .authority(auth)
        .path_and_query("/")
        .build()
        .expect("domain is valid Uri")
}

#[derive(Debug)]
pub struct OnceCell<T>(async_once_cell::OnceCell<T>);

impl<T> OnceCell<T> {
    pub const fn new() -> Self {
        Self(async_once_cell::OnceCell::new())
    }

    pub async fn get_or_try_init<E>(
        &self,
        init: impl Future<Output = std::result::Result<T, E>>,
    ) -> std::result::Result<&T, E> {
        self.0.get_or_try_init(init).await
    }
}

impl<T: Clone> Clone for OnceCell<T> {
    fn clone(&self) -> Self {
        match self.0.get() {
            None => Self::new(),
            Some(v) => Self(async_once_cell::OnceCell::new_with(v.clone())),
        }
    }
}