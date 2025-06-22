#[cfg(any(feature = "native-tls", feature = "rustls"))]
use {compio::tls::TlsConnector, std::io};

/// Represents TLS backend options
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TlsBackend {
    /// Don't use TLS backend.
    None,
    /// Use [`native_tls`] as TLS backend.
    #[cfg(feature = "native-tls")]
    NativeTls,
    /// Use [`rustls`] as TLS backend.
    #[cfg(feature = "rustls")]
    Rustls(Option<std::sync::Arc<compio::tls::rustls::ClientConfig>>),
}

impl Default for TlsBackend {
    fn default() -> Self {
        cfg_if::cfg_if! {
            if #[cfg(feature = "native-tls")] {
                Self::NativeTls
            } else if #[cfg(feature = "rustls")] {
                Self::Rustls(None)
            } else {
                Self::None
            }
        }
    }
}

impl TlsBackend {
    #[cfg(any(feature = "native-tls", feature = "rustls"))]
    pub(crate) fn create_connector(&self) -> io::Result<TlsConnector> {
        match self {
            Self::None => Err(io::Error::other(
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
                    .map_err(io::Error::other)?,
            )),
            #[cfg(feature = "rustls")]
            Self::Rustls(config) => Ok(TlsConnector::from(if let Some(config) = config.clone() {
                config
            } else {
                use std::sync::Arc;

                use compio::rustls::ClientConfig;
                use rustls_platform_verifier::ConfigVerifierExt;

                let mut config =
                    ClientConfig::with_platform_verifier().map_err(io::Error::other)?;
                config.alpn_protocols = if cfg!(feature = "http2") {
                    vec![b"h2".into(), b"http/1.1".into()]
                } else {
                    vec![b"http/1.1".into()]
                };
                config.key_log = Arc::new(compio::rustls::KeyLogFile::new());
                Arc::new(config)
            })),
        }
    }
}
