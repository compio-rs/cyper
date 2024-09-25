#[cfg(any(feature = "native-tls", feature = "rustls"))]
use {compio::tls::TlsConnector, std::io};

/// Represents TLS backend options
#[derive(Debug, Clone)]
pub enum TlsBackend {
    /// Don't use TLS backend.
    None,
    /// Use [`native_tls`] as TLS backend.
    #[cfg(feature = "native-tls")]
    NativeTls,
    /// Use [`rustls`] as TLS backend.
    #[cfg(feature = "rustls")]
    Rustls(std::sync::Arc<compio::tls::rustls::ClientConfig>),
}

impl Default for TlsBackend {
    fn default() -> Self {
        cfg_if::cfg_if! {
            if #[cfg(feature = "native-tls")] {
                Self::NativeTls
            } else if #[cfg(feature = "rustls")] {
                Self::default_rustls()
            } else {
                Self::None
            }
        }
    }
}

impl TlsBackend {
    /// Create [`TlsBackend`] with default rustls client config.
    #[cfg(feature = "rustls")]
    pub fn default_rustls() -> Self {
        let mut config = rustls_platform_verifier::tls_config();
        config.alpn_protocols = if cfg!(feature = "http2") {
            vec![b"h2".into(), b"http/1.1".into()]
        } else {
            vec![b"http/1.1".into()]
        };
        Self::Rustls(std::sync::Arc::new(config))
    }

    #[cfg(any(feature = "native-tls", feature = "rustls"))]
    pub(crate) fn create_connector(&self) -> io::Result<TlsConnector> {
        match self {
            Self::None => Err(io::Error::new(
                io::ErrorKind::Other,
                "could not create TLS connector without TLS backend",
            )),
            #[cfg(feature = "native-tls")]
            Self::NativeTls => Ok(TlsConnector::from(
                compio::tls::native_tls::TlsConnector::builder()
                    .request_alpns(if cfg!(feature = "http2") {
                        &["h2", "http/1.1"]
                    } else {
                        &["http/1.1"]
                    })
                    .build()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?,
            )),
            #[cfg(feature = "rustls")]
            Self::Rustls(config) => Ok(TlsConnector::from(config.clone())),
        }
    }
}
