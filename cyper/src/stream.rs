use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

use compio::{BufResult, net::TcpStream};
use cyper_core::HyperStream;
use futures_util::StreamExt;
use http::HeaderValue;
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};

use crate::{Error, Result, TlsBackend, resolve::ArcResolver};

/// A HTTP stream wrapper, based on compio, and exposes [`hyper::rt`]
/// interfaces.
pub struct HttpStream {
    inner: HyperStream<TcpStream>,
    is_proxy: bool,
}

impl HttpStream {
    /// Create [`HttpStream`] with target uri and TLS backend.
    pub async fn connect(uri: Uri, tls: TlsBackend, resolver: Option<ArcResolver>) -> Result<Self> {
        Self::connect_inner(uri, tls, resolver, false).await
    }

    /// Create [`HttpStream`] as an HTTP forward proxy connection.
    ///
    /// The returned stream will report `is_proxy = true`, telling hyper to
    /// use absolute-form URIs in the request line.
    pub async fn connect_proxy(
        proxy_uri: Uri,
        tls: TlsBackend,
        resolver: Option<ArcResolver>,
    ) -> Result<Self> {
        Self::connect_inner(proxy_uri, tls, resolver, true).await
    }

    /// Connect to the target through an HTTP CONNECT tunnel.
    ///
    /// This is used when HTTPS traffic needs to go through an HTTP proxy.
    /// Connects to the proxy, sends CONNECT, and TLS-wraps the tunneled
    /// connection.
    #[cfg(any(feature = "native-tls", feature = "rustls"))]
    pub async fn connect_tunneled(
        proxy_uri: Uri,
        target_uri: &Uri,
        auth: Option<&HeaderValue>,
        tls: TlsBackend,
        resolver: Option<ArcResolver>,
    ) -> Result<Self> {
        let proxy_host = proxy_uri.host().expect("proxy URI should have host");
        let proxy_port = proxy_uri.port_u16().unwrap_or(8080);

        let target_host = target_uri
            .host()
            .expect("target URI should have host")
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(target_uri.host().unwrap());
        let target_port =
            target_uri
                .port_u16()
                .unwrap_or(if target_uri.scheme_str() == Some("https") {
                    443
                } else {
                    80
                });

        let stream = Self::connect_tcp(&proxy_uri, proxy_host, proxy_port, resolver).await?;
        let stream = Self::do_connect_handshake(stream, target_host, target_port, auth).await?;

        let connector = tls.create_connector()?;
        let tls_stream = connector.connect(target_host, stream).await?;

        Ok(Self {
            inner: HyperStream::new_tls(tls_stream),
            is_proxy: false,
        })
    }

    #[cfg(any(feature = "native-tls", feature = "rustls"))]
    async fn do_connect_handshake(
        stream: TcpStream,
        host: &str,
        port: u16,
        auth: Option<&HeaderValue>,
    ) -> Result<TcpStream> {
        use compio::io::{AsyncReadExt, AsyncWriteExt};

        let auth_line = auth
            .and_then(|v| v.to_str().ok())
            .map(|v| format!("Proxy-Authorization: {v}\r\n"))
            .unwrap_or_default();

        let request =
            format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth_line}\r\n");

        let mut stream = stream;
        let BufResult(res, _) = stream.write_all(request).await;
        res?;

        let mut buf = Vec::<u8>::with_capacity(4096);
        loop {
            let BufResult(res, b) = stream.append(buf).await;
            let n = res?;
            buf = b;
            if buf.len() >= 4 && buf[buf.len() - 4..] == *b"\r\n\r\n" {
                break;
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected eof").into());
            }
        }

        let resp =
            std::str::from_utf8(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let status_line = resp.lines().next().unwrap_or("");
        if !status_line.starts_with("HTTP/1.1 200") && !status_line.starts_with("HTTP/1.0 200") {
            return Err(io::Error::other(format!("proxy CONNECT failed: {status_line}")).into());
        }

        Ok(stream)
    }

    async fn connect_inner(
        uri: Uri,
        tls: TlsBackend,
        resolver: Option<ArcResolver>,
        is_proxy: bool,
    ) -> Result<Self> {
        let scheme = uri.scheme_str().unwrap_or("http");
        let host = uri.host().expect("there should be host");
        // `Uri::host()` includes brackets for IPv6, we must strip them.
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        let port = uri.port_u16();
        let stream = match scheme {
            "http" => {
                let port = port.unwrap_or(80);
                let stream = Self::connect_tcp(&uri, host, port, resolver).await?;
                // Ignore it.
                let _tls = tls;
                HyperStream::new_plain(stream)
            }
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            "https" => {
                let port = port.unwrap_or(443);
                let stream = Self::connect_tcp(&uri, host, port, resolver).await?;
                let connector = tls.create_connector()?;
                HyperStream::new_tls(connector.connect(host, stream).await?)
            }
            _ => return Err(Error::BadScheme(scheme.to_string())),
        };
        Ok(Self {
            inner: stream,
            is_proxy,
        })
    }

    async fn connect_tcp(
        uri: &Uri,
        host: &str,
        port: u16,
        resolver: Option<ArcResolver>,
    ) -> Result<TcpStream> {
        let stream = match resolver {
            None => TcpStream::connect((host, port)).await?,

            Some(resolver) => {
                let addrs = resolver
                    .resolve(uri)
                    .await?
                    .map(|ip| SocketAddr::new(ip, port))
                    .collect::<Vec<_>>()
                    .await;

                TcpStream::connect(addrs.as_slice()).await?
            }
        };

        Ok(stream)
    }
}

impl hyper::rt::Read for HttpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for HttpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl Connection for HttpStream {
    fn connected(&self) -> Connected {
        let conn = Connected::new().proxy(self.is_proxy);
        let is_h2 = self
            .inner
            .negotiated_alpn()
            .map(|alpn| *alpn == *b"h2")
            .unwrap_or_default();
        if is_h2 { conn.negotiated_h2() } else { conn }
    }
}
