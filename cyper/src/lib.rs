//! A high level HTTP client based on compio.

#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod body;
pub use body::*;

mod client;
pub use client::*;

mod request;
pub use request::*;

mod response;
pub use response::*;

mod into_url;
pub use into_url::*;

mod util;

#[cfg(feature = "http3")]
mod http3;

#[cfg(feature = "http3-altsvc")]
mod altsvc;

/// The error type used in `compio-http`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The request is timeout.
    #[error("request timeout")]
    Timeout,
    /// Bad scheme.
    #[error("bad scheme: {0}")]
    BadScheme(url::Url),
    /// IO error occurs.
    #[error("system error: {0}")]
    System(#[from] std::io::Error),
    /// HTTP related parse error.
    #[error("`http` error: {0}")]
    Http(#[from] http::Error),
    /// Hyper error.
    #[error("`hyper` error: {0}")]
    Hyper(#[from] hyper::Error),
    /// Hyper client error.
    #[error("`hyper` client error: {0}")]
    HyperClient(#[from] hyper_util::client::legacy::Error),
    /// URL parse error.
    #[error("url parse error: {0}")]
    UrlParse(#[from] url::ParseError),
    /// URL encoding error.
    #[error("url encode error: {0}")]
    UrlEncoded(#[from] serde_urlencoded::ser::Error),
    /// JSON serialization error.
    #[cfg(feature = "json")]
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// H3 error.
    #[cfg(feature = "http3")]
    #[error("`h3` error: {0}")]
    H3(#[from] h3::Error),
    /// Custom H3 error.
    #[cfg(feature = "http3")]
    #[error("HTTP3 client error: {0}")]
    H3Client(String),
    /// QUIC [`ConnectError`].
    ///
    /// [`ConnectError`]: compio::quic::ConnectError
    #[cfg(feature = "http3")]
    #[error("QUIC connect error: {0}")]
    QuicConnect(#[from] compio::quic::ConnectError),
    /// QUIC [`ConnectionError`].
    ///
    /// [`ConnectionError`]: compio::quic::ConnectionError
    #[cfg(feature = "http3")]
    #[error("QUIC connection error: {0}")]
    QuicConnection(#[from] compio::quic::ConnectionError),
}

/// The result type used in `compio-http`.
pub type Result<T> = std::result::Result<T, Error>;
