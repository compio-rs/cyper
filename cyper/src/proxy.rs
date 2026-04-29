//! Proxy support
//!
//! By default, a `Client` will connect directly to the destination.
//! To route requests through a proxy, configure a `Proxy` and add it to the
//! `ClientBuilder`.
//!
//! # Example
//!
//! ```rust
//! # fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let proxy = cyper::proxy::Proxy::http("http://my-proxy:8080")?;
//! let client = cyper::Client::builder().proxy(proxy).build();
//! # Ok(())
//! # }
//! ```

use std::{error::Error as StdError, fmt, sync::Arc};

use http::{HeaderMap, Uri, header::HeaderValue, uri::Scheme};
use hyper_util::client::proxy::matcher;
use url::Url;

use crate::into_url::IntoUrl;

/// Configuration of a proxy that a `Client` should pass requests to.
///
/// A `Proxy` has a couple pieces to it:
///
/// - a URL of how to talk to the proxy
/// - rules on what `Client` requests should be directed to the proxy
///
/// For instance, let's look at `Proxy::http`:
///
/// ```rust
/// # fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let proxy = cyper::proxy::Proxy::http("https://secure.example")?;
/// # Ok(())
/// # }
/// ```
///
/// Multiple `Proxy` rules can be configured for a `Client`. The `Client` will
/// check each `Proxy` in the order it was added. This could mean that a
/// `Proxy` added first with eager intercept rules, such as `Proxy::all`,
/// would prevent a `Proxy` later in the list from ever working, so take care.
#[derive(Clone)]
pub struct Proxy {
    extra: Extra,
    intercept: Intercept,
    no_proxy: Option<NoProxy>,
}

/// A configuration for filtering out requests that shouldn't be proxied.
#[derive(Clone, Debug, Default)]
pub struct NoProxy {
    inner: String,
}

#[derive(Clone)]
struct Extra {
    auth: Option<HeaderValue>,
    misc: Option<HeaderMap>,
}

/// Trait used for converting into a proxy URL. This trait supports
/// parsing from a URL-like type, whilst also supporting proxy URLs
/// built directly using the factory methods.
pub trait IntoProxy {
    /// Convert the value into a proxy URL.
    fn into_proxy(self) -> crate::Result<Url>;
}

impl IntoProxy for Url {
    fn into_proxy(self) -> crate::Result<Url> {
        if self.port().is_none()
            && matches!(self.scheme(), "socks4" | "socks4a" | "socks5" | "socks5h")
        {
            let mut url = self;
            let _ = url.set_port(Some(1080));
            Ok(url)
        } else {
            Ok(self)
        }
    }
}

impl IntoProxy for &str {
    fn into_proxy(self) -> crate::Result<Url> {
        proxy_from_str(self)
    }
}

impl IntoProxy for String {
    fn into_proxy(self) -> crate::Result<Url> {
        proxy_from_str(&self)
    }
}

impl IntoProxy for &String {
    fn into_proxy(self) -> crate::Result<Url> {
        proxy_from_str(self)
    }
}

fn proxy_from_str(s: &str) -> crate::Result<Url> {
    match s.into_url() {
        Ok(mut url) => {
            if url.port().is_none()
                && matches!(url.scheme(), "socks4" | "socks4a" | "socks5" | "socks5h")
            {
                let _ = url.set_port(Some(1080));
            }
            Ok(url)
        }
        Err(e) => {
            if has_valid_scheme(&e) {
                return Err(e);
            }
            // the issue could have been caused by a missing scheme, so we try adding http://
            let try_this = format!("http://{s}");
            try_this.into_url().map_err(|_| e)
        }
    }
}

fn has_valid_scheme(err: &crate::Error) -> bool {
    let mut source = err.source();
    while let Some(err) = source {
        if let Some(parse_err) = err.downcast_ref::<url::ParseError>()
            && *parse_err == url::ParseError::RelativeUrlWithoutBase
        {
            return false;
        }
        source = err.source();
    }
    true
}

impl Proxy {
    /// Proxy all HTTP traffic to the passed URL.
    pub fn http<U: IntoProxy>(proxy_scheme: U) -> crate::Result<Proxy> {
        Ok(Proxy::new(Intercept::Http(proxy_scheme.into_proxy()?)))
    }

    /// Proxy all HTTPS traffic to the passed URL.
    pub fn https<U: IntoProxy>(proxy_scheme: U) -> crate::Result<Proxy> {
        Ok(Proxy::new(Intercept::Https(proxy_scheme.into_proxy()?)))
    }

    /// Proxy **all** traffic to the passed URL.
    ///
    /// "All" refers to `https` and `http` URLs.
    pub fn all<U: IntoProxy>(proxy_scheme: U) -> crate::Result<Proxy> {
        Ok(Proxy::new(Intercept::All(proxy_scheme.into_proxy()?)))
    }

    /// Provide a custom function to determine what traffic to proxy to where.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn run() -> Result<(), cyper::Error> {
    /// let target = url::Url::parse("https://my.prox")?;
    /// let client = cyper::Client::builder()
    ///     .proxy(cyper::proxy::Proxy::custom(move |url| {
    ///         if url.host_str() == Some("cyper") {
    ///             Some(target.clone())
    ///         } else {
    ///             None
    ///         }
    ///     }))
    ///     .build();
    /// # Ok(())
    /// # }
    /// ```
    pub fn custom<F, U: IntoProxy>(fun: F) -> Proxy
    where
        F: Fn(&Url) -> Option<U> + Send + Sync + 'static,
    {
        Proxy::new(Intercept::Custom(Custom {
            func: Arc::new(move |url| fun(url).map(IntoProxy::into_proxy)),
            no_proxy: None,
        }))
    }

    fn new(intercept: Intercept) -> Proxy {
        Proxy {
            extra: Extra {
                auth: None,
                misc: None,
            },
            intercept,
            no_proxy: None,
        }
    }

    /// Set the `Proxy-Authorization` header using Basic auth.
    pub fn basic_auth(mut self, username: &str, password: &str) -> Proxy {
        match self.intercept {
            Intercept::All(ref mut s)
            | Intercept::Http(ref mut s)
            | Intercept::Https(ref mut s) => url_auth(s, username, password),
            Intercept::Custom(_) => {
                let header = encode_basic_auth(username, password);
                self.extra.auth = Some(header);
            }
        }

        self
    }

    /// Set the `Proxy-Authorization` header to a specified value.
    pub fn custom_http_auth(mut self, header_value: HeaderValue) -> Proxy {
        self.extra.auth = Some(header_value);
        self
    }

    /// Adds custom headers to this Proxy.
    pub fn headers(mut self, headers: HeaderMap) -> Proxy {
        self.extra.misc = Some(headers);
        self
    }

    /// Adds a `No Proxy` exclusion list to this Proxy.
    pub fn no_proxy(mut self, no_proxy: Option<NoProxy>) -> Proxy {
        self.no_proxy = no_proxy;
        self
    }

    pub(crate) fn into_matcher(self) -> Matcher {
        let Proxy {
            intercept,
            extra,
            no_proxy,
        } = self;

        let maybe_has_http_auth;
        let maybe_has_http_custom_headers;

        let inner = match intercept {
            Intercept::All(url) => {
                maybe_has_http_auth = cache_maybe_has_http_auth(&url, &extra.auth);
                maybe_has_http_custom_headers =
                    cache_maybe_has_http_custom_headers(&url, &extra.misc);
                Matcher_::Util(
                    matcher::Matcher::builder()
                        .all(String::from(url))
                        .no(no_proxy.as_ref().map(|n| n.inner.as_ref()).unwrap_or(""))
                        .build(),
                )
            }
            Intercept::Http(url) => {
                maybe_has_http_auth = cache_maybe_has_http_auth(&url, &extra.auth);
                maybe_has_http_custom_headers =
                    cache_maybe_has_http_custom_headers(&url, &extra.misc);
                Matcher_::Util(
                    matcher::Matcher::builder()
                        .http(String::from(url))
                        .no(no_proxy.as_ref().map(|n| n.inner.as_ref()).unwrap_or(""))
                        .build(),
                )
            }
            Intercept::Https(url) => {
                maybe_has_http_auth = cache_maybe_has_http_auth(&url, &extra.auth);
                maybe_has_http_custom_headers =
                    cache_maybe_has_http_custom_headers(&url, &extra.misc);
                Matcher_::Util(
                    matcher::Matcher::builder()
                        .https(String::from(url))
                        .no(no_proxy.as_ref().map(|n| n.inner.as_ref()).unwrap_or(""))
                        .build(),
                )
            }
            Intercept::Custom(mut custom) => {
                maybe_has_http_auth = true; // never know
                maybe_has_http_custom_headers = true;
                custom.no_proxy = no_proxy;
                Matcher_::Custom(custom)
            }
        };

        Matcher {
            inner,
            extra,
            maybe_has_http_auth,
            maybe_has_http_custom_headers,
        }
    }
}

fn cache_maybe_has_http_auth(url: &Url, extra: &Option<HeaderValue>) -> bool {
    (url.scheme() == "http" || url.scheme() == "https")
        && (!url.username().is_empty() || url.password().is_some() || extra.is_some())
}

fn cache_maybe_has_http_custom_headers(url: &Url, extra: &Option<HeaderMap>) -> bool {
    (url.scheme() == "http" || url.scheme() == "https") && extra.is_some()
}

impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Proxy")
            .field(&self.intercept)
            .field(&self.no_proxy)
            .finish()
    }
}

impl NoProxy {
    /// Returns a new no-proxy configuration based on environment variables
    /// (or `None` if no variables are set).
    ///
    /// See [`Self::from_string()`] for the string format.
    pub fn from_env() -> Option<NoProxy> {
        let raw = std::env::var("NO_PROXY")
            .or_else(|_| std::env::var("no_proxy"))
            .ok()?;

        Some(Self::from_string(&raw).unwrap_or_default())
    }

    /// Returns a new no-proxy configuration based on a `no_proxy` string.
    ///
    /// The rules are as follows:
    /// * Entries are expected to be comma-separated (whitespace between entries
    ///   is ignored)
    /// * IP addresses (both IPv4 and IPv6) are allowed, as are optional subnet
    ///   masks (by adding /size, for example "`192.168.1.0/24`")
    /// * An entry "`*`" matches all hostnames (this is the only wildcard
    ///   allowed)
    /// * Any other entry is considered a domain name (and may contain a leading
    ///   dot, for example `google.com` and `.google.com` are equivalent) and
    ///   would match both that domain AND all subdomains.
    ///
    /// For example, if `"NO_PROXY=google.com, 192.168.1.0/24"` was set, all the
    /// following would match (and therefore would bypass the proxy):
    /// * `http://google.com/`
    /// * `http://www.google.com/`
    /// * `http://192.168.1.42/`
    ///
    /// The URL `http://notgoogle.com/` would not match.
    pub fn from_string(no_proxy_list: &str) -> Option<Self> {
        Some(NoProxy {
            inner: no_proxy_list.into(),
        })
    }
}

// ===== Internal =====

pub(crate) struct Matcher {
    inner: Matcher_,
    extra: Extra,
    maybe_has_http_auth: bool,
    maybe_has_http_custom_headers: bool,
}

#[allow(clippy::large_enum_variant)]
enum Matcher_ {
    Util(matcher::Matcher),
    Custom(Custom),
}

/// The result of matching a URL against proxy rules.
pub(crate) struct Intercepted {
    inner: matcher::Intercept,
    extra: Extra,
}

impl Matcher {
    pub(crate) fn intercept(&self, dst: &Uri) -> Option<Intercepted> {
        let inner = match self.inner {
            Matcher_::Util(ref m) => m.intercept(dst),
            Matcher_::Custom(ref c) => c.call(dst),
        };

        inner.map(|inner| Intercepted {
            inner,
            extra: self.extra.clone(),
        })
    }

    /// Return whether this matcher might provide HTTP (not SOCKS) auth.
    ///
    /// This is a hint to allow skipping a more expensive check if this proxy
    /// will never need auth when forwarding.
    pub(crate) fn maybe_has_http_auth(&self) -> bool {
        self.maybe_has_http_auth
    }

    /// Get the `Proxy-Authorization` header for a non-tunnel HTTP forward
    /// proxy.
    pub(crate) fn http_non_tunnel_basic_auth(&self, dst: &Uri) -> Option<HeaderValue> {
        if let Some(proxy) = self.intercept(dst) {
            let scheme = proxy.uri().scheme();
            if scheme == Some(&Scheme::HTTP) || scheme == Some(&Scheme::HTTPS) {
                return proxy.basic_auth().cloned();
            }
        }

        None
    }

    pub(crate) fn maybe_has_http_custom_headers(&self) -> bool {
        self.maybe_has_http_custom_headers
    }

    pub(crate) fn http_non_tunnel_custom_headers(&self, dst: &Uri) -> Option<HeaderMap> {
        if let Some(proxy) = self.intercept(dst) {
            let scheme = proxy.uri().scheme();
            if scheme == Some(&Scheme::HTTP) || scheme == Some(&Scheme::HTTPS) {
                return proxy.custom_headers().cloned();
            }
        }

        None
    }
}

impl fmt::Debug for Matcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inner {
            Matcher_::Util(ref m) => m.fmt(f),
            Matcher_::Custom(ref m) => m.fmt(f),
        }
    }
}

impl Intercepted {
    pub(crate) fn uri(&self) -> &Uri {
        self.inner.uri()
    }

    pub(crate) fn basic_auth(&self) -> Option<&HeaderValue> {
        if let Some(ref val) = self.extra.auth {
            return Some(val);
        }
        self.inner.basic_auth()
    }

    pub(crate) fn custom_headers(&self) -> Option<&HeaderMap> {
        if let Some(ref val) = self.extra.misc {
            return Some(val);
        }
        None
    }
}

impl fmt::Debug for Intercepted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.uri().fmt(f)
    }
}

#[derive(Clone, Debug)]
enum Intercept {
    All(Url),
    Http(Url),
    Https(Url),
    Custom(Custom),
}

fn url_auth(url: &mut Url, username: &str, password: &str) {
    let _ = url.set_username(username);
    let _ = url.set_password(Some(password));
}

#[derive(Clone)]
struct Custom {
    func: Arc<dyn Fn(&Url) -> Option<crate::Result<Url>> + Send + Sync + 'static>,
    no_proxy: Option<NoProxy>,
}

impl Custom {
    fn call(&self, uri: &Uri) -> Option<matcher::Intercept> {
        let url = format!(
            "{}://{}{}{}",
            uri.scheme()?,
            uri.host()?,
            uri.port().map_or("", |_| ":"),
            uri.port().map_or(String::new(), |p| p.to_string())
        )
        .parse()
        .expect("should be valid Url");

        (self.func)(&url)
            .and_then(|result| result.ok())
            .and_then(|target| {
                let m = matcher::Matcher::builder()
                    .all(String::from(target))
                    .build();

                m.intercept(uri)
            })
    }
}

impl fmt::Debug for Custom {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("_")
    }
}

pub(crate) fn encode_basic_auth(username: &str, password: &str) -> HeaderValue {
    crate::util::basic_auth(username, Some(password))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Uri {
        s.parse().unwrap()
    }

    fn intercepted_uri(p: &Matcher, s: &str) -> Uri {
        p.intercept(&s.parse().unwrap()).unwrap().uri().clone()
    }

    #[test]
    fn test_http() {
        let target = "http://example.domain/";
        let p = Proxy::http(target).unwrap().into_matcher();

        let http = "http://cyper";
        let other = "https://cyper";

        assert_eq!(intercepted_uri(&p, http), target);
        assert!(p.intercept(&url(other)).is_none());
    }

    #[test]
    fn test_https() {
        let target = "http://example.domain/";
        let p = Proxy::https(target).unwrap().into_matcher();

        let http = "http://cyper";
        let other = "https://cyper";

        assert!(p.intercept(&url(http)).is_none());
        assert_eq!(intercepted_uri(&p, other), target);
    }

    #[test]
    fn test_all() {
        let target = "http://example.domain/";
        let p = Proxy::all(target).unwrap().into_matcher();

        let http = "http://cyper";
        let https = "https://cyper";

        assert_eq!(intercepted_uri(&p, http), target);
        assert_eq!(intercepted_uri(&p, https), target);
    }

    #[test]
    fn test_custom() {
        let target1 = "http://example.domain/";
        let target2 = "https://example.domain/";
        let p = Proxy::custom(move |url| {
            if url.host_str() == Some("cyper") {
                target1.parse().ok()
            } else if url.scheme() == "http" {
                target2.parse().ok()
            } else {
                None::<Url>
            }
        })
        .into_matcher();

        let http = "http://seanmonstar.com";
        let https = "https://cyper";
        let other = "x-youve-never-heard-of-me-mr-proxy://seanmonstar.com";

        assert_eq!(intercepted_uri(&p, http), target2);
        assert_eq!(intercepted_uri(&p, https), target1);
        assert!(p.intercept(&url(other)).is_none());
    }

    #[test]
    fn test_standard_with_custom_auth_header() {
        let target = "http://example.domain/";
        let p = Proxy::all(target)
            .unwrap()
            .custom_http_auth(HeaderValue::from_static("testme"))
            .into_matcher();

        let got = p.intercept(&url("http://anywhere.local")).unwrap();
        let auth = got.basic_auth().unwrap();
        assert_eq!(auth, "testme");
    }

    #[test]
    fn test_custom_with_custom_auth_header() {
        let target = "http://example.domain/";
        let p = Proxy::custom(move |_| target.parse::<Url>().ok())
            .custom_http_auth(HeaderValue::from_static("testme"))
            .into_matcher();

        let got = p.intercept(&url("http://anywhere.local")).unwrap();
        let auth = got.basic_auth().unwrap();
        assert_eq!(auth, "testme");
    }

    #[test]
    fn test_maybe_has_http_auth() {
        let m = Proxy::all("https://letme:in@yo.local")
            .unwrap()
            .into_matcher();
        assert!(m.maybe_has_http_auth(), "https forwards");

        let m = Proxy::all("http://letme:in@yo.local")
            .unwrap()
            .into_matcher();
        assert!(m.maybe_has_http_auth(), "http forwards");

        let m = Proxy::all("http://:in@yo.local").unwrap().into_matcher();
        assert!(m.maybe_has_http_auth(), "http forwards with empty username");

        let m = Proxy::all("http://letme:@yo.local").unwrap().into_matcher();
        assert!(m.maybe_has_http_auth(), "http forwards with empty password");
    }

    #[test]
    fn test_socks_proxy_default_port() {
        {
            let m = Proxy::all("socks5://example.com").unwrap().into_matcher();

            let http = "http://cyper";
            let https = "https://cyper";

            assert_eq!(intercepted_uri(&m, http).port_u16(), Some(1080));
            assert_eq!(intercepted_uri(&m, https).port_u16(), Some(1080));

            // custom port
            let m = Proxy::all("socks5://example.com:1234")
                .unwrap()
                .into_matcher();

            assert_eq!(intercepted_uri(&m, http).port_u16(), Some(1234));
            assert_eq!(intercepted_uri(&m, https).port_u16(), Some(1234));
        }
    }

    #[test]
    fn test_https_matcher_matches_https() {
        let m = Proxy::https("http://127.0.0.1:8080")
            .unwrap()
            .into_matcher();
        let dst: Uri = "https://cyper.local/prox".parse().unwrap();
        assert!(
            m.intercept(&dst).is_some(),
            "HTTPS proxy should match HTTPS URL"
        );
    }

    #[test]
    fn test_https_matcher_skips_http() {
        let m = Proxy::https("http://127.0.0.1:8080")
            .unwrap()
            .into_matcher();
        let dst: Uri = "http://cyper.local/prox".parse().unwrap();
        assert!(
            m.intercept(&dst).is_none(),
            "HTTPS proxy should not match HTTP URL"
        );
    }

    #[test]
    fn test_all_matcher_matches_both() {
        let m = Proxy::all("http://127.0.0.1:8080").unwrap().into_matcher();
        let http_dst: Uri = "http://cyper.local/prox".parse().unwrap();
        let https_dst: Uri = "https://cyper.local/prox".parse().unwrap();
        assert!(
            m.intercept(&http_dst).is_some(),
            "All proxy should match HTTP URL"
        );
        assert!(
            m.intercept(&https_dst).is_some(),
            "All proxy should match HTTPS URL"
        );
    }

    #[test]
    fn test_https_matcher_with_credentials() {
        let m = Proxy::https("http://Aladdin:open sesame@127.0.0.1:8080")
            .unwrap()
            .into_matcher();
        let dst: Uri = "https://cyper.local/prox".parse().unwrap();
        assert!(
            m.intercept(&dst).is_some(),
            "HTTPS proxy with credentials should match HTTPS URL"
        );
    }

    #[test]
    fn test_http_matcher_with_credentials() {
        let m = Proxy::http("http://Aladdin:open sesame@127.0.0.1:8080")
            .unwrap()
            .into_matcher();
        let dst: Uri = "http://cyper.local/prox".parse().unwrap();
        assert!(
            m.intercept(&dst).is_some(),
            "HTTP proxy with credentials should match HTTP URL"
        );
    }
}
