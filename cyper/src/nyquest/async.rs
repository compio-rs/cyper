use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue};
use http_body_util::BodyExt;
use nyquest_interface::{
    Request, Result,
    r#async::{AsyncBackend, AsyncClient, AsyncResponse, BoxedStream},
    client::ClientOptions,
};
use send_wrapper::SendWrapper;
use url::Url;

use super::CyperBackend;

impl AsyncBackend for CyperBackend {
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
        Ok(CyperAsyncClient {
            client,
            base_url,
            user_agent: options.user_agent,
            timeout: options.request_timeout,
            max_buffer_size: options.max_response_buffer_size,
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
    max_buffer_size: Option<u64>,
}

impl AsyncClient for CyperAsyncClient {
    type Response = CyperAsyncResponse;

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
        let resp = SendWrapper::new(fut).await?;
        Ok(CyperAsyncResponse {
            resp,
            max_buffer_size: self.max_buffer_size,
        })
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

/// An implementation of [`nyquest_interface::r#async::AsyncResponse`].
pub struct CyperAsyncResponse {
    resp: crate::Response,
    max_buffer_size: Option<u64>,
}

impl futures_util::AsyncRead for CyperAsyncResponse {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.resp.poll_next_unpin(cx) {
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

impl AsyncResponse for CyperAsyncResponse {
    fn status(&self) -> u16 {
        self.resp.status().as_u16()
    }

    fn content_length(&self) -> Option<u64> {
        self.resp.content_length()
    }

    fn get_header(&self, header: &str) -> Result<Vec<String>> {
        Ok(self
            .resp
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

    async fn bytes(mut self: Pin<&mut Self>) -> Result<Vec<u8>> {
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
