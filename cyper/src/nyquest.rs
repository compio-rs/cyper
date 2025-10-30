//! Adapters for [`nyquest_interface`].
//!
//! This support is experimental. Not all features are implemented.
//! It might break regardless of semver in future releases.

use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures_util::{Stream, StreamExt, TryStreamExt};
use http::{HeaderMap, HeaderName, HeaderValue};
use nyquest_interface::{
    Request, Result,
    r#async::{AsyncBackend, AsyncClient, AsyncResponse, BoxedStream},
    client::ClientOptions,
};
use send_wrapper::SendWrapper;
use url::Url;

/// Register `cyper` as a backend for [`nyquest_interface`].
pub fn register() {
    nyquest_interface::register_backend(CyperAsyncBackend);
}

/// An implementation of [`nyquest_interface::r#async::AsyncBackend`].
///
/// ## Missing features
/// * `caching_behavior`
/// * `use_default_proxy`
/// * `follow_redirects`
/// * `max_response_buffer_size`
/// * `ignore_certificate_errors`
pub struct CyperAsyncBackend;

impl AsyncBackend for CyperAsyncBackend {
    type AsyncClient = CyperAsyncClient;

    async fn create_async_client(&self, options: ClientOptions) -> Result<Self::AsyncClient> {
        let builder = crate::ClientBuilder::new().default_headers(HeaderMap::from_iter(
            options.default_headers.into_iter().map(|(k, v)| {
                (
                    HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(&v).unwrap(),
                )
            }),
        ));
        #[cfg(feature = "cookies")]
        let builder = builder.cookie_store(options.use_cookies);
        let client = builder.build();
        let base_url = if let Some(url) = options.base_url {
            Some(Url::parse(&url).map_err(|_| nyquest_interface::Error::InvalidUrl)?)
        } else {
            None
        };
        Ok(CyperAsyncClient {
            client,
            base_url,
            user_agent: options.user_agent,
            timeout: options.request_timeout,
        })
    }
}

/// An implementation of [`nyquest_interface::r#async::AsyncClient`].
#[derive(Clone)]
pub struct CyperAsyncClient {
    client: crate::Client,
    user_agent: Option<String>,
    base_url: Option<Url>,
    timeout: Option<Duration>,
}

impl AsyncClient for CyperAsyncClient {
    type Response = crate::Response;

    async fn request(&self, req: Request<BoxedStream>) -> Result<Self::Response> {
        let fut = async {
            let method = match req.method {
                nyquest_interface::Method::Delete => http::Method::DELETE,
                nyquest_interface::Method::Get => http::Method::GET,
                nyquest_interface::Method::Head => http::Method::HEAD,
                nyquest_interface::Method::Patch => http::Method::PATCH,
                nyquest_interface::Method::Post => http::Method::POST,
                nyquest_interface::Method::Put => http::Method::PUT,
                nyquest_interface::Method::Other(s) => {
                    http::Method::from_bytes(s.as_bytes()).unwrap()
                }
            };
            let url = match self.base_url.as_ref() {
                Some(base) => base.join(&req.relative_uri),
                None => Url::parse(&req.relative_uri),
            }
            .map_err(|_| nyquest_interface::Error::InvalidUrl)?;
            let builder = self
                .client
                .request(method, url)?
                .headers(HeaderMap::from_iter(
                    req.additional_headers.into_iter().map(|(k, v)| {
                        (
                            HeaderName::from_bytes(k.as_bytes()).unwrap(),
                            HeaderValue::from_str(&v).unwrap(),
                        )
                    }),
                ));
            let (body, content_type) = match req.body {
                Some(body) => match body {
                    nyquest_interface::Body::Bytes {
                        content,
                        content_type,
                    } => (
                        crate::body::Body::from(content.into_owned()),
                        Some(content_type),
                    ),
                    nyquest_interface::Body::Stream {
                        stream,
                        content_type,
                    } => (
                        crate::body::Body::stream(WrapBoxedStream(stream)),
                        Some(content_type),
                    ),
                    _ => {
                        return Err(nyquest_interface::Error::Io(std::io::Error::other(
                            "Unsupported body type",
                        )));
                    }
                },
                None => (crate::body::Body::empty(), None),
            };
            let builder = builder.body(body);
            let builder = if let Some(content_type) = content_type {
                builder.header(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_str(&content_type).unwrap(),
                )?
            } else {
                builder
            };
            let builder = if let Some(user_agent) = &self.user_agent {
                builder.header(
                    http::header::USER_AGENT,
                    HeaderValue::from_str(user_agent).unwrap(),
                )?
            } else {
                builder
            };
            if let Some(timeout) = self.timeout {
                Ok(compio::time::timeout(timeout, builder.send())
                    .await
                    .map_err(|_| nyquest_interface::Error::RequestTimeout)??)
            } else {
                Ok(builder.send().await?)
            }
        };
        SendWrapper::new(fut).await
    }
}

struct WrapBoxedStream(BoxedStream);

impl futures_util::Stream for WrapBoxedStream {
    type Item = crate::Result<compio::bytes::Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut buffer = [0u8; 1024];
        let s = std::pin::pin!(&mut self.get_mut().0);
        match futures_util::AsyncRead::poll_read(s, cx, &mut buffer) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(0)) => Poll::Ready(None),
            Poll::Ready(Ok(n)) => {
                let bytes = compio::bytes::Bytes::copy_from_slice(&buffer[..n]);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e.into()))),
        }
    }
}

impl futures_util::AsyncRead for crate::Response {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(bytes))) => {
                let len = bytes.len().min(buf.len());
                buf[..len].copy_from_slice(&bytes[..len]);
                Poll::Ready(Ok(len))
            }
            Poll::Ready(Some(Err(e))) => match e {
                crate::Error::System(io_err) => Poll::Ready(Err(io_err)),
                other_err => Poll::Ready(Err(std::io::Error::other(format!(
                    "Response body read error: {}",
                    other_err
                )))),
            },
            Poll::Ready(None) => Poll::Ready(Ok(0)),
        }
    }
}

impl AsyncResponse for crate::Response {
    fn status(&self) -> u16 {
        self.status().as_u16()
    }

    fn content_length(&self) -> Option<u64> {
        self.content_length()
    }

    fn get_header(&self, header: &str) -> Result<Vec<String>> {
        Ok(self
            .headers()
            .get_all(header)
            .into_iter()
            .filter_map(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .collect())
    }

    async fn text(self: Pin<&mut Self>) -> Result<String> {
        self.bytes()
            .await
            .map(|b| String::from_utf8_lossy(&b).to_string())
    }

    async fn bytes(self: Pin<&mut Self>) -> Result<Vec<u8>> {
        Ok(self
            .map(|res| match res {
                Ok(bytes) => Result::Ok(bytes.to_vec()),
                Err(e) => Err(e.into()),
            })
            .try_collect::<Vec<Vec<u8>>>()
            .await?
            .into_iter()
            .flatten()
            .collect())
    }
}

impl From<crate::Error> for nyquest_interface::Error {
    fn from(err: crate::Error) -> Self {
        match err {
            crate::Error::BadScheme(_) => nyquest_interface::Error::InvalidUrl,
            crate::Error::System(e) => nyquest_interface::Error::Io(e),
            crate::Error::Timeout => nyquest_interface::Error::RequestTimeout,
            _ => {
                nyquest_interface::Error::Io(std::io::Error::other(format!("cyper error: {}", err)))
            }
        }
    }
}
