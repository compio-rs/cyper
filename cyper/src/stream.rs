use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use compio::{buf::IoBuf, net::TcpStream, tls::MaybeTlsStream};
use cyper_core::HyperStream;
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};

use crate::{Error, Result, TlsBackend};

/// A HTTP stream wrapper, based on compio, and exposes [`hyper::rt`]
/// interfaces.
pub struct HttpStream(HyperStream<MaybeTlsStream<TcpStream>>);

impl HttpStream {
    /// Create [`HttpStream`] with target uri and TLS backend.
    pub async fn connect(uri: Uri, tls: TlsBackend) -> Result<Self> {
        let scheme = uri.scheme_str().unwrap_or("http");
        let host = uri.host().expect("there should be host");
        let port = uri.port_u16();
        let stream = match scheme {
            "http" => {
                let stream = TcpStream::connect((host, port.unwrap_or(80))).await?;
                // Ignore it.
                let _tls = tls;
                MaybeTlsStream::new_plain(stream)
            }
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            "https" => {
                let stream = TcpStream::connect((host, port.unwrap_or(443))).await?;
                let connector = tls.create_connector()?;
                MaybeTlsStream::new_tls(connector.connect(host, stream).await?)
            }
            _ => return Err(Error::BadScheme(scheme.to_string())),
        };
        Ok(Self(HyperStream::new(stream)))
    }
}

impl hyper::rt::Read for HttpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let inner = std::pin::pin!(&mut self.0);
        inner.poll_read(cx, buf)
    }
}

impl hyper::rt::Write for HttpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let inner = std::pin::pin!(&mut self.0);
        inner.poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner = std::pin::pin!(&mut self.0);
        inner.poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner = std::pin::pin!(&mut self.0);
        inner.poll_shutdown(cx)
    }
}

impl Connection for HttpStream {
    fn connected(&self) -> Connected {
        let conn = Connected::new();
        let is_h2 = self
            .0
            .get_ref()
            .negotiated_alpn()
            .map(|alpn| alpn.as_slice() == b"h2")
            .unwrap_or_default();
        if is_h2 { conn.negotiated_h2() } else { conn }
    }
}
