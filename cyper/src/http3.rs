#[cfg(feature = "once_cell_try")]
use std::sync::OnceLock;
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, TryRecvError},
    },
    time::Instant,
};

use compio::{
    buf::bytes::Bytes,
    net::{ToSocketAddrsAsync, UdpSocket},
    quic::{
        ClientBuilder, ConnectError, Connecting, Connection, Endpoint, EndpointConfig,
        h3::{OpenStreams, client::SendRequest},
    },
    runtime::Runtime,
};
use futures_util::TryStreamExt;
use h3::error::ConnectionError;
use http::{
    Request, Uri,
    uri::{Authority, Scheme},
};
use http_body_util::BodyDataStream;
use hyper::body::Buf;
#[cfg(not(feature = "once_cell_try"))]
use once_cell::sync::OnceCell as OnceLock;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use url::Url;

use crate::{Body, Error, Response, Result};

#[derive(Debug)]
struct DualEndpoint {
    v4end: Option<Endpoint>,
    v6end: Endpoint,
}

impl DualEndpoint {
    fn client_builder() -> Result<ClientBuilder<compio::rustls::ClientConfig>> {
        Ok(ClientBuilder::new_with_platform_verifier()?
            .with_key_log()
            .with_alpn_protocols(&["h3"]))
    }

    fn new() -> Result<Self> {
        let client_config = Self::client_builder()?.build();

        let v6sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        let dual_stack = v6sock.set_only_v6(false).is_ok();
        v6sock.bind(&SockAddr::from(SocketAddrV6::new(
            Ipv6Addr::UNSPECIFIED,
            0,
            0,
            0,
        )))?;
        let is_polling = Runtime::with_current(|r| r.driver_type().is_polling());
        if is_polling {
            v6sock.set_nonblocking(true)?;
        }
        let v6sock = UdpSocket::from_std(v6sock.into())?;
        let v6end = Endpoint::new(
            v6sock,
            EndpointConfig::default(),
            None,
            Some(client_config.clone()),
        )?;
        let v4end = if dual_stack {
            None
        } else {
            let v4sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            v4sock.bind(&SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)))?;
            if is_polling {
                v4sock.set_nonblocking(true)?;
            }
            let v4sock = UdpSocket::from_std(v4sock.into())?;
            Some(Endpoint::new(
                v4sock,
                EndpointConfig::default(),
                None,
                Some(client_config),
            )?)
        };

        Ok(Self { v4end, v6end })
    }

    fn end(&self, is_v4: bool) -> &Endpoint {
        if let Some(v4end) = &self.v4end {
            if is_v4 {
                return v4end;
            }
        }
        &self.v6end
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
    endpoint: Arc<OnceLock<DualEndpoint>>,
}

impl Connector {
    pub fn new() -> Self {
        Self {
            endpoint: Arc::new(OnceLock::new()),
        }
    }

    fn endpoint(&self) -> Result<&DualEndpoint> {
        self.endpoint.get_or_try_init(DualEndpoint::new)
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

        let endpoint = self.endpoint()?;

        let mut err = None;
        for remote in (host, port).to_socket_addrs_async().await? {
            match Self::connect_impl(endpoint, remote, server_name).await {
                Ok(conn) => return Ok(compio::quic::h3::client::new(conn).await?),
                Err(e) => err = Some(e),
            }
        }
        Err(err.unwrap_or_else(|| {
            Error::H3Client("failed to establish connection for HTTP/3 request".into())
        }))
    }

    async fn connect_impl(
        endpoint: &DualEndpoint,
        remote: SocketAddr,
        server_name: &str,
    ) -> Result<Connection> {
        Ok(endpoint.connect(remote, server_name)?.await?)
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

        let mut req_body = BodyDataStream::new(req_body);
        while let Some(b) = req_body.try_next().await? {
            stream.send_data(b).await?;
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
    close_rx: Receiver<ConnectionError>,
    client: PoolClient,
    idle_timeout: Instant,
}

impl PoolConnection {
    pub fn new(client: PoolClient, close_rx: Receiver<ConnectionError>) -> Self {
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
            let e = driver.wait_idle().await;
            close_tx.send(e).ok();
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
    pub fn new() -> Self {
        Self {
            pool: Pool::new(),
            connector: Connector::new(),
        }
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

    pub async fn request(&self, mut req: Request<Body>, url: Url) -> Result<Response> {
        let pool_key = extract_domain(req.uri_mut())?;
        self.clone().send_request(pool_key, req, url).await
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
