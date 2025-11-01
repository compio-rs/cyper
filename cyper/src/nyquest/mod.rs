//! Adapters for [`nyquest_interface`].
//!
//! This support is experimental. Not all features are implemented.
//! It might break regardless of semver in future releases.

use std::time::Duration;

use compio::bytes::Bytes;
use futures_util::Stream;
use http::{HeaderMap, HeaderName, HeaderValue};
use http_body_util::BodyExt;
use nyquest_interface::{Request, Result, client::ClientOptions};
use send_wrapper::SendWrapper;
use url::Url;

/// Register `cyper` as a backend for [`nyquest_interface`].
pub fn register() {
    nyquest_interface::register_backend(CyperBackend);
}

#[cfg(feature = "nyquest-async")]
mod r#async;

#[cfg(feature = "nyquest-blocking")]
mod blocking;

/// An implementation of nyquest backend.
///
/// ## Missing features
/// * `caching_behavior`
/// * `use_default_proxy`
/// * `follow_redirects`
pub struct CyperBackend;

impl CyperBackend {
    pub(crate) fn create_client(&self, options: ClientOptions) -> Result<CyperClient> {
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
        let builder = if options.ignore_certificate_errors {
            builder.danger_accept_invalid_certs(true)
        } else {
            builder
        };
        let client = builder.build();
        let base_url = if let Some(url) = options.base_url {
            Some(Url::parse(&url).map_err(|_| nyquest_interface::Error::InvalidUrl)?)
        } else {
            None
        };
        Ok(CyperClient {
            client,
            base_url,
            user_agent: options.user_agent,
            timeout: options.request_timeout,
            max_buffer_size: options.max_response_buffer_size,
        })
    }
}

/// An implementation of nyquest client.
#[derive(Clone)]
pub struct CyperClient {
    client: crate::Client,
    user_agent: Option<String>,
    base_url: Option<Url>,
    timeout: Option<Duration>,
    max_buffer_size: Option<u64>,
}

impl CyperClient {
    pub(crate) async fn request<S: Stream<Item = crate::Result<Bytes>> + Send + 'static>(
        &self,
        req: Request<S>,
    ) -> Result<CyperResponse> {
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
                    } => (crate::body::Body::stream(stream), Some(content_type)),
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
        let resp = SendWrapper::new(fut).await?;
        Ok(CyperResponse {
            resp,
            max_buffer_size: self.max_buffer_size,
        })
    }
}

/// An implementation of nyquest response.
pub struct CyperResponse {
    resp: crate::Response,
    max_buffer_size: Option<u64>,
}

impl CyperResponse {
    pub(crate) fn status(&self) -> u16 {
        self.resp.status().as_u16()
    }

    pub(crate) fn content_length(&self) -> Option<u64> {
        self.resp.content_length()
    }

    pub(crate) fn get_header(&self, header: &str) -> Result<Vec<String>> {
        Ok(self
            .resp
            .headers()
            .get_all(header)
            .into_iter()
            .filter_map(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .collect())
    }

    pub(crate) async fn bytes_impl(&mut self) -> Result<Vec<u8>> {
        let mut bufs = vec![];
        let mut collected_size = 0;
        loop {
            let Some(frame) = self.resp.body.frame().await else {
                break;
            };
            let Ok(frame) = frame?.into_data() else {
                continue;
            };
            if self
                .max_buffer_size
                .is_some_and(|max| collected_size + frame.len() > max as usize)
            {
                return Err(nyquest_interface::Error::ResponseTooLarge);
            }
            collected_size += frame.len();
            bufs.push(frame);
        }
        Ok(bufs.concat())
    }
}

fn to_io_error(err: crate::Error) -> std::io::Error {
    match err {
        crate::Error::System(e) => e,
        other_err => std::io::Error::other(format!("nyquest error: {}", other_err)),
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
