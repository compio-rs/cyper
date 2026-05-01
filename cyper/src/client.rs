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
    Body, Connector, IntoUrl, Request, RequestBuilder, Response, Result, TlsBackend, proxy,
    redirect,
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

        let mut res = self.send_request(request, &url).await?;

        // Redirect loop
        let mut current_url = url;
        let mut prev_urls: Vec<Url> = Vec::new();

        loop {
            #[cfg(feature = "cookies")]
            self.store_response_cookies(&current_url, &res);

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

            current_url = next_url;

            match action {
                redirect::ActionKind::Follow => {
                    redirect::remove_sensitive_headers(
                        &mut redirect_headers,
                        &current_url,
                        &prev_urls,
                    );

                    if self.client.referer
                        && let Some(previous_url) = prev_urls.last()
                    {
                        if let Some(v) = redirect::make_referer(&current_url, previous_url) {
                            redirect_headers.insert(http::header::REFERER, v);
                        } else {
                            redirect_headers.remove(http::header::REFERER);
                        }
                    }

                    match status {
                        StatusCode::MOVED_PERMANENTLY
                        | StatusCode::FOUND
                        | StatusCode::SEE_OTHER => {
                            if matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND) {
                                method = Method::GET;
                            }
                            body_backup = Some(Body::empty());
                            redirect_headers.remove(http::header::CONTENT_LENGTH);
                            redirect_headers.remove(http::header::CONTENT_TYPE);
                            redirect_headers.remove(http::header::TRANSFER_ENCODING);
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
                            current_url
                                .as_str()
                                .parse::<Uri>()
                                .expect("a parsed Url should always be a valid Uri"),
                        )
                        .version(version)
                        .body(send_body)?;
                    let mut req = req;
                    *req.headers_mut() = redirect_headers.clone();

                    res = self.send_request(req, &current_url).await?;
                }
                redirect::ActionKind::Stop => return Ok(res),
                redirect::ActionKind::Error(e) => return Err(crate::Error::Redirect(e)),
            }
        }
    }

    #[allow(unused_mut)]
    async fn send_request(&self, mut request: http::Request<Body>, url: &Url) -> Result<Response> {
        let uri = request.uri().clone();
        self.proxy_auth(&uri, request.headers_mut());
        self.proxy_custom_headers(&uri, request.headers_mut());
        self.accept_header(request.headers_mut());

        #[cfg(feature = "http3")]
        {
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
                self.send_h1h2_request(request, url).await?
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
            Ok(res)
        }
        #[cfg(not(feature = "http3"))]
        {
            self.send_h1h2_request(request, url).await
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

    fn proxy_auth(&self, dst: &Uri, headers: &mut HeaderMap) {
        if !self.client.proxies_maybe_http_auth {
            return;
        }

        // Only set the header here if the destination scheme is 'http',
        // since otherwise, the header will be included in the CONNECT tunnel
        // request instead.
        if dst.scheme() != Some(&http::uri::Scheme::HTTP) {
            return;
        }

        if headers.contains_key(http::header::PROXY_AUTHORIZATION) {
            return;
        }

        for proxy in self.client.proxies.iter() {
            if let Some(header) = proxy.http_non_tunnel_basic_auth(dst) {
                headers.insert(http::header::PROXY_AUTHORIZATION, header);
                break;
            }
        }
    }

    fn proxy_custom_headers(&self, dst: &Uri, headers: &mut HeaderMap) {
        if !self.client.proxies_maybe_http_custom_headers {
            return;
        }

        if dst.scheme() != Some(&http::uri::Scheme::HTTP) {
            return;
        }

        for proxy in self.client.proxies.iter() {
            if let Some(iter) = proxy.http_non_tunnel_custom_headers(dst) {
                iter.iter().for_each(|(key, value)| {
                    headers.insert(key, value.clone());
                });
                break;
            }
        }
    }

    fn accept_header(&self, headers: &mut HeaderMap) {
        if !headers.contains_key(http::header::ACCEPT) {
            headers.insert(http::header::ACCEPT, http::HeaderValue::from_static("*/*"));
        }
        if headers.contains_key(http::header::ACCEPT_ENCODING)
            || headers.contains_key(http::header::RANGE)
        {
            return;
        }
        if let Some(value) = self.client.accepts.clone() {
            headers.insert(http::header::ACCEPT_ENCODING, value);
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
    referer: bool,
    proxies: Arc<Vec<proxy::Matcher>>,
    proxies_maybe_http_auth: bool,
    proxies_maybe_http_custom_headers: bool,
    #[cfg(feature = "cookies")]
    cookies: Option<RwLock<CookieStore>>,
    accepts: Option<HeaderValue>,
}

#[derive(Clone, Copy, Debug)]
struct Accepts {
    #[cfg(feature = "gzip")]
    gzip: bool,
    #[cfg(feature = "brotli")]
    brotli: bool,
    #[cfg(feature = "zstd")]
    zstd: bool,
    #[cfg(feature = "deflate")]
    deflate: bool,
}

impl Default for Accepts {
    fn default() -> Accepts {
        Accepts {
            #[cfg(feature = "gzip")]
            gzip: true,
            #[cfg(feature = "brotli")]
            brotli: true,
            #[cfg(feature = "zstd")]
            zstd: true,
            #[cfg(feature = "deflate")]
            deflate: true,
        }
    }
}

impl Accepts {
    pub fn header_value(&self) -> Option<HeaderValue> {
        #[allow(unused_mut)]
        let mut encodings = Vec::<&'static str>::new();
        #[cfg(feature = "gzip")]
        if self.gzip {
            encodings.push("gzip");
        }
        #[cfg(feature = "brotli")]
        if self.brotli {
            encodings.push("br");
        }
        #[cfg(feature = "zstd")]
        if self.zstd {
            encodings.push("zstd");
        }
        #[cfg(feature = "deflate")]
        if self.deflate {
            encodings.push("deflate");
        }

        encodings.join(", ").parse().ok()
    }
}

/// A `ClientBuilder` can be used to create a `Client` with custom
/// configuration.
#[derive(Debug)]
#[must_use]
pub struct ClientBuilder {
    accepts: Accepts,
    tls: TlsBackend,
    headers: HeaderMap,
    resolver: Option<ArcResolver>,
    redirect_policy: redirect::Policy,
    referer: bool,
    proxies: Vec<proxy::Proxy>,
    no_proxy: bool,
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
            accepts: Accepts::default(),
            headers: HeaderMap::new(),
            tls: TlsBackend::default(),
            resolver: None,
            redirect_policy: redirect::Policy::default(),
            referer: true,
            proxies: Vec::new(),
            no_proxy: false,
            #[cfg(feature = "cookies")]
            cookies: None,
        }
    }

    /// Returns a `Client` that uses this `ClientBuilder` configuration.
    pub fn build(self) -> Client {
        #[allow(unused)]
        let accept_invalid_certs = self.tls.accept_invalid_certs();
        let proxies = if self.no_proxy {
            Arc::new(vec![])
        } else if !self.proxies.is_empty() {
            Arc::new(self.proxies.into_iter().map(|p| p.into_matcher()).collect())
        } else {
            Arc::new(vec![proxy::Matcher::system()])
        };
        let client = hyper_util::client::legacy::Client::builder(CompioExecutor)
            .set_host(true)
            .timer(CompioTimer)
            .build(Connector::new(
                self.tls,
                self.resolver.clone(),
                proxies.clone(),
            ));

        let proxies_maybe_http_auth = proxies.iter().any(|p| p.maybe_has_http_auth());
        let proxies_maybe_http_custom_headers =
            proxies.iter().any(|p| p.maybe_has_http_custom_headers());
        let client_ref = ClientInner {
            client,
            headers: self.headers,
            redirect_policy: self.redirect_policy,
            referer: self.referer,
            proxies,
            proxies_maybe_http_auth,
            proxies_maybe_http_custom_headers,
            #[cfg(feature = "cookies")]
            cookies: self.cookies,
            accepts: self.accepts.header_value(),
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

    /// Enable or disable setting a `Referer` header on redirects.
    ///
    /// Default is `true`.
    pub fn referer(mut self, enable: bool) -> Self {
        self.referer = enable;
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

    /// Add a `Proxy` to the list of proxies the `Client` will use.
    ///
    /// # Note
    ///
    /// Adding a proxy will disable the automatic usage of the "system" proxy.
    pub fn proxy(mut self, proxy: proxy::Proxy) -> Self {
        self.proxies.push(proxy);
        self
    }

    /// Clear all `Proxies`, so `Client` will use no proxy anymore.
    ///
    /// # Note
    /// To add a proxy exclusion list, use [crate::proxy::Proxy::no_proxy()]
    /// on all desired proxies instead.
    ///
    /// This also disables the automatic usage of the "system" proxy.
    pub fn no_proxy(mut self) -> Self {
        self.no_proxy = true;
        self
    }

    /// Set the custom resolver for DNS resolution.
    pub fn custom_resolver<R: Resolve + 'static>(mut self, resolver: R) -> Self {
        self.resolver = Some(ArcResolver::new(resolver));
        self
    }

    /// Enable auto gzip decompression by checking the `Content-Encoding`
    /// response header.
    ///
    /// If auto gzip decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already
    ///   contain an `Accept-Encoding` **and** `Range` values, the
    ///   `Accept-Encoding` header is set to `gzip`. The request body is **not**
    ///   automatically compressed.
    /// - When receiving a response, if its headers contain a `Content-Encoding`
    ///   value of `gzip`, both `Content-Encoding` and `Content-Length` are
    ///   removed from the headers' set. The response body is automatically
    ///   decompressed.
    ///
    /// If the `gzip` feature is turned on, the default option is enabled.
    #[cfg(feature = "gzip")]
    pub fn gzip(mut self, enable: bool) -> Self {
        self.accepts.gzip = enable;
        self
    }

    /// Enable auto brotli decompression by checking the `Content-Encoding`
    /// response header.
    ///
    /// If auto brotli decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already
    ///   contain an `Accept-Encoding` **and** `Range` values, the
    ///   `Accept-Encoding` header is set to `br`. The request body is **not**
    ///   automatically compressed.
    /// - When receiving a response, if its headers contain a `Content-Encoding`
    ///   value of `br`, both `Content-Encoding` and `Content-Length` are
    ///   removed from the headers' set. The response body is automatically
    ///   decompressed.
    ///
    /// If the `brotli` feature is turned on, the default option is enabled.
    #[cfg(feature = "brotli")]
    pub fn brotli(mut self, enable: bool) -> Self {
        self.accepts.brotli = enable;
        self
    }

    /// Enable auto zstd decompression by checking the `Content-Encoding`
    /// response header.
    ///
    /// If auto zstd decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already
    ///   contain an `Accept-Encoding` **and** `Range` values, the
    ///   `Accept-Encoding` header is set to `zstd`. The request body is **not**
    ///   automatically compressed.
    /// - When receiving a response, if its headers contain a `Content-Encoding`
    ///   value of `zstd`, both `Content-Encoding` and `Content-Length` are
    ///   removed from the headers' set. The response body is automatically
    ///   decompressed.
    ///
    /// If the `zstd` feature is turned on, the default option is enabled.
    #[cfg(feature = "zstd")]
    pub fn zstd(mut self, enable: bool) -> Self {
        self.accepts.zstd = enable;
        self
    }

    /// Enable auto deflate decompression by checking the `Content-Encoding`
    /// response header.
    ///
    /// If auto deflate decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already
    ///   contain an `Accept-Encoding` **and** `Range` values, the
    ///   `Accept-Encoding` header is set to `deflate`. The request body is
    ///   **not** automatically compressed.
    /// - When receiving a response, if it's headers contain a
    ///   `Content-Encoding` value that equals to `deflate`, both values
    ///   `Content-Encoding` and `Content-Length` are removed from the headers'
    ///   set. The response body is automatically decompressed.
    ///
    /// If the `deflate` feature is turned on, the default option is enabled.
    #[cfg(feature = "deflate")]
    pub fn deflate(mut self, enable: bool) -> Self {
        self.accepts.deflate = enable;
        self
    }

    /// Disable auto response body gzip decompression.
    ///
    /// This method exists even if the optional `gzip` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use gzip decompression
    /// even if another dependency were to enable the optional `gzip` feature.
    pub fn no_gzip(self) -> Self {
        #[cfg(feature = "gzip")]
        {
            self.gzip(false)
        }

        #[cfg(not(feature = "gzip"))]
        {
            self
        }
    }

    /// Disable auto response body brotli decompression.
    ///
    /// This method exists even if the optional `brotli` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use brotli decompression
    /// even if another dependency were to enable the optional `brotli` feature.
    pub fn no_brotli(self) -> Self {
        #[cfg(feature = "brotli")]
        {
            self.brotli(false)
        }

        #[cfg(not(feature = "brotli"))]
        {
            self
        }
    }

    /// Disable auto response body zstd decompression.
    ///
    /// This method exists even if the optional `zstd` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use zstd decompression
    /// even if another dependency were to enable the optional `zstd` feature.
    pub fn no_zstd(self) -> Self {
        #[cfg(feature = "zstd")]
        {
            self.zstd(false)
        }

        #[cfg(not(feature = "zstd"))]
        {
            self
        }
    }

    /// Disable auto response body deflate decompression.
    ///
    /// This method exists even if the optional `deflate` feature is not
    /// enabled. This can be used to ensure a `Client` doesn't use deflate
    /// decompression even if another dependency were to enable the optional
    /// `deflate` feature.
    pub fn no_deflate(self) -> Self {
        #[cfg(feature = "deflate")]
        {
            self.deflate(false)
        }

        #[cfg(not(feature = "deflate"))]
        {
            self
        }
    }
}

#[cfg(all(feature = "impl_trait_in_assoc_type", feature = "stream"))]
impl tower_service::Service<http::Request<Body>> for &Client {
    type Error = crate::Error;
    type Response = http::Response<Body>;

    type Future = impl Future<Output = Result<http::Response<Body>>> + use<>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let client = self.clone();
        async move { client.execute_tower(req).await }
    }
}

#[cfg(all(not(feature = "impl_trait_in_assoc_type"), feature = "stream"))]
impl tower_service::Service<http::Request<Body>> for &Client {
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

#[cfg(feature = "stream")]
impl tower_service::Service<http::Request<Body>> for Client {
    type Error = crate::Error;
    type Future = <&'static Client as tower_service::Service<http::Request<Body>>>::Future;
    type Response = http::Response<Body>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        (&*self).call(req)
    }
}
