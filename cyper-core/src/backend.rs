use std::io;

use compio::tls::TlsConnector;

/// Represents TLS backend options
#[derive(Debug, Clone)]
pub enum TlsBackend {
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
                compile_error!("You must choose at least one of these features: [\"native-tls\", \"rustls\"]")
            }
        }
    }
}

impl TlsBackend {
    /// Create [`TlsBackend`] with default rustls client config.
    #[cfg(feature = "rustls")]
    pub fn default_rustls() -> Self {
        let mut store = compio::tls::rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().unwrap() {
            store.add(cert).unwrap();
        }

        Self::Rustls(std::sync::Arc::new(
            compio::tls::rustls::ClientConfig::builder()
                .with_root_certificates(store)
                .with_no_client_auth(),
        ))
    }

    pub(crate) fn create_connector(&self) -> io::Result<TlsConnector> {
        match self {
            #[cfg(feature = "native-tls")]
            Self::NativeTls => Ok(TlsConnector::from(
                compio::tls::native_tls::TlsConnector::new()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?,
            )),
            #[cfg(feature = "rustls")]
            Self::Rustls(config) => Ok(TlsConnector::from(config.clone())),
        }
    }
}
