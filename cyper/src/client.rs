use std::sync::Arc;

use cyper_core::{CompioExecutor, CompioTimer, Connector, TlsBackend};
use http::header::Entry;
use hyper::{HeaderMap, Method, Uri};
#[cfg(feature = "cookies")]
use {
    compio::bytes::Bytes, cookie_store::CookieStore, http::HeaderValue, std::sync::RwLock, url::Url,
};

use crate::{Body, IntoUrl, Request, RequestBuilder, Response, Result};

/// An asynchronous `Client` to make Requests with.
#[derive(Debug, Clone)]
pub struct Client {
    client: Arc<ClientInner>,
}

impl Client {
    /// Create a client with default config.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        ClientBuilder::new().build()
    }

    /// Creates a `ClientBuilder` to configure a `Client`.
    ///
    /// This is the same as `ClientBuilder::new()`.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Send a request and wait for a response.
    pub async fn execute(&self, request: Request) -> Result<Response> {
        let (method, url, mut headers, body, version) = request.pieces();

        for (key, value) in &self.client.headers {
            if let Entry::Vacant(entry) = headers.entry(key) {
                entry.insert(value.clone());
            }
        }

        #[cfg(feature = "cookies")]
        {
            if headers.get(http::header::COOKIE).is_none() {
                if let Some(cookie_store) = self.cookie_value(&url) {
                    headers.insert(http::header::COOKIE, cookie_store);
                }
            }
        }

        let mut request = hyper::Request::builder()
            .method(method)
            .uri(
                url.as_str()
                    .parse::<Uri>()
                    .expect("a parsed Url should always be a valid Uri"),
            )
            .version(version)
            .body(body)?;
        *request.headers_mut() = headers;

        let res = self.client.client.request(request).await?;

        #[cfg(feature = "cookies")]
        {
            if let Some(cookie_store) = &self.client.cookies {
                let mut values = res
                    .headers()
                    .get_all(http::header::SET_COOKIE)
                    .into_iter()
                    .peekable();
                if values.peek().is_some() {
                    let mut cookie_store = cookie_store.write().unwrap();
                    cookie_store.store_response_cookies(
                        values.filter_map(|val| {
                            std::str::from_utf8(val.as_bytes()).ok()?.parse().ok()
                        }),
                        &url,
                    );
                }
            }
        }

        Ok(Response::new(res, url))
    }

    #[cfg(feature = "cookies")]
    fn cookie_value(&self, url: &Url) -> Option<HeaderValue> {
        let cookie_store = self.client.cookies.as_ref()?.read().unwrap();
        let value = cookie_store
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        if !value.is_empty() {
            Some(HeaderValue::from_maybe_shared(Bytes::from(value)).ok()?)
        } else {
            None
        }
    }

    /// Send a request with method and url.
    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> Result<RequestBuilder> {
        Ok(RequestBuilder::new(
            self.clone(),
            Request::new(method, url.into_url()?),
        ))
    }

    /// Convenience method to make a `GET` request to a URL.
    pub fn get<U: IntoUrl>(&self, url: U) -> Result<RequestBuilder> {
        self.request(Method::GET, url)
    }

    /// Convenience method to make a `POST` request to a URL.
    pub fn post<U: IntoUrl>(&self, url: U) -> Result<RequestBuilder> {
        self.request(Method::POST, url)
    }

    /// Convenience method to make a `PUT` request to a URL.
    pub fn put<U: IntoUrl>(&self, url: U) -> Result<RequestBuilder> {
        self.request(Method::PUT, url)
    }

    /// Convenience method to make a `PATCH` request to a URL.
    pub fn patch<U: IntoUrl>(&self, url: U) -> Result<RequestBuilder> {
        self.request(Method::PATCH, url)
    }

    /// Convenience method to make a `DELETE` request to a URL.
    pub fn delete<U: IntoUrl>(&self, url: U) -> Result<RequestBuilder> {
        self.request(Method::DELETE, url)
    }

    /// Convenience method to make a `HEAD` request to a URL.
    pub fn head<U: IntoUrl>(&self, url: U) -> Result<RequestBuilder> {
        self.request(Method::HEAD, url)
    }
}

#[derive(Debug)]
struct ClientInner {
    client: hyper_util::client::legacy::Client<Connector, Body>,
    headers: HeaderMap,
    #[cfg(feature = "cookies")]
    cookies: Option<RwLock<CookieStore>>,
}

/// A `ClientBuilder` can be used to create a `Client` with custom
/// configuration.
#[derive(Debug)]
#[must_use]
pub struct ClientBuilder {
    tls: TlsBackend,
    headers: HeaderMap,
    #[cfg(feature = "cookies")]
    cookies: Option<RwLock<CookieStore>>,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientBuilder {
    /// Constructs a new `ClientBuilder`.
    pub fn new() -> Self {
        Self {
            headers: HeaderMap::new(),
            tls: TlsBackend::default(),
            #[cfg(feature = "cookies")]
            cookies: None,
        }
    }

    /// Returns a `Client` that uses this `ClientBuilder` configuration.
    pub fn build(self) -> Client {
        let client = hyper_util::client::legacy::Client::builder(CompioExecutor)
            .set_host(true)
            .timer(CompioTimer)
            .build(Connector::new(self.tls));
        let client_ref = ClientInner {
            client,
            headers: self.headers,
            #[cfg(feature = "cookies")]
            cookies: self.cookies,
        };
        Client {
            client: Arc::new(client_ref),
        }
    }

    /// Set the default headers for every request.
    pub fn default_headers(mut self, headers: HeaderMap) -> Self {
        for (key, value) in headers.iter() {
            self.headers.insert(key, value.clone());
        }
        self
    }

    /// Force using the native TLS backend.
    #[cfg(feature = "native-tls")]
    pub fn use_native_tls(mut self) -> Self {
        self.tls = TlsBackend::NativeTls;
        self
    }

    /// Force using the Rustls TLS backend.
    #[cfg(feature = "rustls")]
    pub fn use_rustls_default(mut self) -> Self {
        self.tls = TlsBackend::default_rustls();
        self
    }

    /// Force using the Rustls TLS backend.
    #[cfg(feature = "rustls")]
    pub fn use_rustls(mut self, config: std::sync::Arc<compio::tls::rustls::ClientConfig>) -> Self {
        self.tls = TlsBackend::Rustls(config);
        self
    }

    /// Enable a persistent cookie store for the client.
    ///
    /// Cookies received in responses will be preserved and included in
    /// additional requests.
    ///
    /// By default, no cookie store is used.
    #[cfg(feature = "cookies")]
    pub fn cookie_store(mut self, enable: bool) -> Self {
        if enable {
            self.cookies = Some(RwLock::new(CookieStore::default()))
        } else {
            self.cookies = None;
        }
        self
    }
}
