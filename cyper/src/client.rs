use std::sync::Arc;

use cyper_core::{CompioExecutor, CompioTimer, Connector, TlsBackend};
use http::header::Entry;
use hyper::{HeaderMap, Method, Uri};
use url::Url;
#[cfg(feature = "cookies")]
use {compio::bytes::Bytes, cookie_store::CookieStore, http::HeaderValue, std::sync::RwLock};

use crate::{Body, IntoUrl, Request, RequestBuilder, Response, Result};

/// An asynchronous `Client` to make Requests with.
#[derive(Debug, Clone)]
pub struct Client {
    client: Arc<ClientInner>,
    #[cfg(feature = "http3")]
    h3_client: crate::http3::Client,
    #[cfg(feature = "http3-altsvc")]
    h3_hosts: crate::altsvc::KnownHosts,
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
        if headers.get(http::header::COOKIE).is_none()
            && let Some(cookie_store) = self.cookie_value_impl(&url)
        {
            headers.insert(http::header::COOKIE, cookie_store);
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

        let res = if cfg!(feature = "http3") {
            #[cfg(feature = "http3-altsvc")]
            let host = url.host_str().expect("a parsed Url should have host");

            #[allow(unused_mut)]
            let mut should_http3 = request.version() == http::Version::HTTP_3;

            #[cfg(feature = "http3-altsvc")]
            if url.port().is_none() && self.h3_hosts.find(host) {
                if let Ok(value) = http::HeaderValue::from_bytes(host.as_bytes()) {
                    request.headers_mut().insert("Alt-Used", value);
                }

                should_http3 = true;
            }

            let res = if should_http3 {
                self.h3_client.request(request, url.clone()).await?
            } else {
                self.send_h1h2_request(request, &url).await?
            };

            #[cfg(feature = "http3-altsvc")]
            if let Some(alt_svc) = res.headers().get(http::header::ALT_SVC)
                && let Ok(alt_svc) = std::str::from_utf8(alt_svc.as_bytes())
                && let Ok(services) = crate::altsvc::parse(alt_svc)
            {
                match services {
                    crate::altsvc::AltService::Clear => self.h3_hosts.clear(host),
                    crate::altsvc::AltService::Services(services) => {
                        for srv in services {
                            if self.h3_hosts.try_insert(host.to_string(), &srv) {
                                break;
                            }
                        }
                    }
                }
            }
            res
        } else {
            self.send_h1h2_request(request, &url).await?
        };

        #[cfg(feature = "cookies")]
        if let Some(cookie_store) = &self.client.cookies {
            let mut values = res
                .headers()
                .get_all(http::header::SET_COOKIE)
                .into_iter()
                .peekable();

            if values.peek().is_some() {
                let mut cookie_store = cookie_store.write().unwrap();
                cookie_store.store_response_cookies(
                    values
                        .map(HeaderValue::as_bytes)
                        .map(str::from_utf8)
                        .filter_map(std::result::Result::ok)
                        .map(str::parse)
                        .filter_map(std::result::Result::ok),
                    &url,
                );
            }
        }

        Ok(res)
    }

    async fn send_h1h2_request(&self, request: http::Request<Body>, url: &Url) -> Result<Response> {
        let res = self.client.client.request(request).await?;
        Ok(Response::new(res, url.clone()))
    }

    /// Get stored cookie value for specified URL. If the URL is valid while no
    /// value found, it returns `Ok(None)`.
    #[cfg(feature = "cookies")]
    pub fn cookie_value<U: IntoUrl>(&self, url: U) -> Result<Option<HeaderValue>> {
        Ok(self.cookie_value_impl(&url.into_url()?))
    }

    #[cfg(feature = "cookies")]
    fn cookie_value_impl(&self, url: &Url) -> Option<HeaderValue> {
        let cookie_store = self.client.cookies.as_ref()?.read().unwrap();
        let value = cookie_store
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");

        (!value.is_empty())
            .then(|| HeaderValue::from_maybe_shared(Bytes::from(value)))
            .and_then(std::result::Result::ok)
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
            #[cfg(feature = "http3")]
            h3_client: crate::http3::Client::new(),
            #[cfg(feature = "http3-altsvc")]
            h3_hosts: crate::altsvc::KnownHosts::default(),
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
        self.tls = TlsBackend::Rustls(None);
        self
    }

    /// Force using the Rustls TLS backend.
    #[cfg(feature = "rustls")]
    pub fn use_rustls(mut self, config: std::sync::Arc<compio::tls::rustls::ClientConfig>) -> Self {
        self.tls = TlsBackend::Rustls(Some(config));
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
        self.cookies = if enable {
            Some(RwLock::new(CookieStore::default()))
        } else {
            None
        };

        self
    }
}
