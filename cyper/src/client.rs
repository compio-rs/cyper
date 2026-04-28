use std::sync::Arc;
#[cfg(feature = "stream")]
use std::task::{Context, Poll};

use cyper_core::{CompioExecutor, CompioTimer};
use http::{HeaderValue, header::Entry};
use hyper::{HeaderMap, Method, StatusCode, Uri};
use url::Url;
#[cfg(feature = "cookies")]
use {compio::bytes::Bytes, cookie_store::CookieStore, std::sync::RwLock};

use crate::{
    Body, Connector, IntoUrl, Request, RequestBuilder, Response, Result, TlsBackend, redirect,
    resolve::{ArcResolver, Resolve},
};

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
        let (method, url, headers, body, version) = request.pieces();

        let request = hyper::Request::builder()
            .method(method)
            .uri(
                url.as_str()
                    .parse::<Uri>()
                    .expect("a parsed Url should always be a valid Uri"),
            )
            .version(version)
            .body(body)?;
        self.execute_impl(request, headers, url).await
    }

    #[cfg(feature = "stream")]
    async fn execute_tower(&self, request: http::Request<Body>) -> Result<http::Response<Body>> {
        let url = request.uri().to_string().parse::<Url>()?;
        let resp = self.execute_impl(request, HeaderMap::new(), url).await?;
        let http_resp = resp.res;
        let body = resp.body;
        Ok(http::Response::from_parts(
            http_resp.into_parts().0,
            body.into(),
        ))
    }

    #[cfg(feature = "cookies")]
    fn store_response_cookies(&self, url: &Url, res: &Response) {
        if let Some(cookie_store) = &self.client.cookies {
            let mut values = res
                .headers()
                .get_all(http::header::SET_COOKIE)
                .into_iter()
                .peekable();
            if values.peek().is_some() {
                let mut cookie_store = cookie_store.write().unwrap();
                cookie_store.store_response_cookies(
                    values.filter_map(|val| std::str::from_utf8(val.as_bytes()).ok()?.parse().ok()),
                    url,
                );
            }
        }
    }

    async fn execute_impl(
        &self,
        mut request: http::Request<Body>,
        mut headers: HeaderMap<HeaderValue>,
        url: Url,
    ) -> Result<Response> {
        for (key, value) in &self.client.headers {
            if let Entry::Vacant(entry) = headers.entry(key) {
                entry.insert(value.clone());
            }
        }

        #[cfg(feature = "cookies")]
        {
            if headers.get(http::header::COOKIE).is_none()
                && let Some(cookie_store) = self.cookie_value_impl(&url)
            {
                headers.insert(http::header::COOKIE, cookie_store);
            }
        }

        *request.headers_mut() = headers;

        // Save state for potential redirects before request is consumed
        let mut method = request.method().clone();
        let version = request.version();
        let mut body_backup = request.body().try_clone();
        let mut redirect_headers = request.headers().clone();

        #[cfg(feature = "http3")]
        let mut res = {
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
                            if self.h3_hosts.try_insert(host, &srv) {
                                break;
                            }
                        }
                    }
                }
            }
            res
        };
        #[cfg(not(feature = "http3"))]
        let mut res = self.send_h1h2_request(request, &url).await?;

        #[cfg(feature = "cookies")]
        self.store_response_cookies(&url, &res);

        // Redirect loop
        let mut current_url = url;
        let mut prev_urls: Vec<Url> = Vec::new();

        loop {
            let status = res.status();
            if !status.is_redirection() {
                return Ok(res);
            }

            let Some(location) = res
                .headers()
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            else {
                return Ok(res);
            };

            let Ok(next_url) = current_url.join(location) else {
                return Ok(res);
            };

            prev_urls.push(current_url);

            let action = self
                .client
                .redirect_policy
                .check(status, &next_url, &prev_urls);

            match action {
                redirect::ActionKind::Follow => {
                    if next_url.scheme() != "http" && next_url.scheme() != "https" {
                        return Err(crate::Error::BadScheme(next_url.scheme().to_owned()));
                    }

                    redirect::remove_sensitive_headers(
                        &mut redirect_headers,
                        &next_url,
                        &prev_urls,
                    );

                    match status {
                        StatusCode::MOVED_PERMANENTLY
                        | StatusCode::FOUND
                        | StatusCode::SEE_OTHER => {
                            method = Method::GET;
                            body_backup = Some(Body::empty());
                        }
                        StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => {
                            if body_backup.is_none() {
                                return Ok(res);
                            }
                        }
                        _ => return Ok(res),
                    }

                    let send_body = body_backup
                        .as_ref()
                        .and_then(|b| b.try_clone())
                        .unwrap_or_default();

                    let req = http::Request::builder()
                        .method(method.clone())
                        .uri(
                            next_url
                                .as_str()
                                .parse::<Uri>()
                                .expect("a parsed Url should always be a valid Uri"),
                        )
                        .version(version)
                        .body(send_body)?;
                    let mut req = req;
                    *req.headers_mut() = redirect_headers.clone();

                    current_url = next_url;
                    res = self.send_h1h2_request(req, &current_url).await?;

                    #[cfg(feature = "cookies")]
                    self.store_response_cookies(&current_url, &res);
                }
                redirect::ActionKind::Stop => return Ok(res),
                redirect::ActionKind::Error(e) => {
                    return Err(crate::Error::Redirect(e));
                }
            }
        }
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
    redirect_policy: redirect::Policy,
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
    resolver: Option<ArcResolver>,
    redirect_policy: redirect::Policy,
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
            resolver: None,
            redirect_policy: redirect::Policy::default(),
            #[cfg(feature = "cookies")]
            cookies: None,
        }
    }

    /// Returns a `Client` that uses this `ClientBuilder` configuration.
    pub fn build(self) -> Client {
        #[allow(unused)]
        let accept_invalid_certs = self.tls.accept_invalid_certs();
        let client = hyper_util::client::legacy::Client::builder(CompioExecutor)
            .set_host(true)
            .timer(CompioTimer)
            .build(Connector::new(self.tls, self.resolver.clone()));
        let client_ref = ClientInner {
            client,
            headers: self.headers,
            redirect_policy: self.redirect_policy,
            #[cfg(feature = "cookies")]
            cookies: self.cookies,
        };
        Client {
            client: Arc::new(client_ref),
            #[cfg(feature = "http3")]
            h3_client: crate::http3::Client::new(accept_invalid_certs, self.resolver),
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
        self.tls = self.tls.with_native_tls();
        self
    }

    /// Force using the Rustls TLS backend.
    #[cfg(feature = "rustls")]
    pub fn use_rustls_default(mut self) -> Self {
        self.tls = self.tls.with_rustls();
        self
    }

    /// Force using the Rustls TLS backend.
    #[cfg(feature = "rustls")]
    pub fn use_rustls(mut self, config: std::sync::Arc<compio::tls::rustls::ClientConfig>) -> Self {
        self.tls = self.tls.with_rustls_config(config);
        self
    }

    /// Controls the use of certificate validation.
    pub fn danger_accept_invalid_certs(mut self, accept: bool) -> Self {
        self.tls = self.tls.with_accept_invalid_certs(accept);
        self
    }

    /// Set the redirect policy.
    ///
    /// Default is `redirect::Policy::default()` which limits to 10 redirects.
    pub fn redirect(mut self, policy: redirect::Policy) -> Self {
        self.redirect_policy = policy;
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

    /// Set the custom resolver for DNS resolution.
    pub fn custom_resolver<R: Resolve + 'static>(mut self, resolver: R) -> Self {
        self.resolver = Some(ArcResolver::new(resolver));
        self
    }
}

#[cfg(feature = "impl_trait_in_assoc_type")]
impl hyper::service::Service<Request> for Client {
    type Error = crate::Error;
    type Response = Response;

    type Future = impl Future<Output = Result<Response>>;

    fn call(&self, req: Request) -> Self::Future {
        let client = self.clone();
        async move { client.execute(req).await }
    }
}

#[cfg(all(feature = "impl_trait_in_assoc_type", feature = "stream"))]
impl tower_service::Service<http::Request<Body>> for Client {
    type Error = crate::Error;
    type Response = http::Response<Body>;

    type Future = impl Future<Output = Result<http::Response<Body>>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let client = self.clone();
        async move { client.execute_tower(req).await }
    }
}

#[cfg(not(feature = "impl_trait_in_assoc_type"))]
impl hyper::service::Service<Request> for Client {
    type Error = crate::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Response>>>>;
    type Response = Response;

    fn call(&self, req: Request) -> Self::Future {
        let client = self.clone();
        Box::pin(async move { client.execute(req).await })
    }
}

#[cfg(all(not(feature = "impl_trait_in_assoc_type"), feature = "stream"))]
impl tower_service::Service<http::Request<Body>> for Client {
    type Error = crate::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<http::Response<Body>>>>>;
    type Response = http::Response<Body>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let client = self.clone();
        Box::pin(async move { client.execute_tower(req).await })
    }
}
