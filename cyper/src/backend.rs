#[cfg(feature = "rustls")]
use compio::rustls;
#[cfg(tls)]
use {
    crate::{Error, Result},
    compio::tls::TlsConnector,
    std::sync::Arc,
    synchrony::sync::mutex::Mutex,
};

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum TlsBackendInner {
    None,
    #[cfg(feature = "native-tls")]
    NativeTls,
    #[cfg(feature = "rustls")]
    Rustls(Option<Arc<rustls::ClientConfig>>),
}

#[allow(clippy::derivable_impls)]
impl Default for TlsBackendInner {
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

#[cfg(tls)]
type SharedTlsConnector = Arc<Mutex<Option<Arc<TlsConnector>>>>;

#[cfg(not(tls))]
type SharedTlsConnector = ();

/// Represents TLS backend options.
#[derive(Debug, Clone, Default)]
pub struct TlsBackend {
    #[allow(dead_code)]
    ty: TlsBackendInner,
    accept_invalid_certs: bool,
    connector: SharedTlsConnector,
}

impl TlsBackend {
    /// Sets the TLS backend to native-tls.
    #[cfg(feature = "native-tls")]
    pub fn with_native_tls(self) -> Self {
        Self {
            ty: TlsBackendInner::NativeTls,
            accept_invalid_certs: self.accept_invalid_certs,
            ..Default::default()
        }
    }

    /// Sets the TLS backend to rustls.
    #[cfg(feature = "rustls")]
    pub fn with_rustls(self) -> Self {
        Self {
            ty: TlsBackendInner::Rustls(None),
            accept_invalid_certs: self.accept_invalid_certs,
            ..Default::default()
        }
    }

    /// Sets the TLS backend to rustls with a custom configuration.
    #[cfg(feature = "rustls")]
    pub fn with_rustls_config(self, config: Arc<rustls::ClientConfig>) -> Self {
        Self {
            ty: TlsBackendInner::Rustls(Some(config)),
            accept_invalid_certs: self.accept_invalid_certs,
            ..Default::default()
        }
    }

    /// Sets whether to accept invalid certificates.
    pub fn with_accept_invalid_certs(mut self, accept: bool) -> Self {
        self.accept_invalid_certs = accept;
        self.connector = Default::default();
        self
    }

    /// Returns whether invalid certificates are accepted.
    pub fn accept_invalid_certs(&self) -> bool {
        self.accept_invalid_certs
    }

    #[cfg(tls)]
    fn build_connector(&self) -> Result<TlsConnector> {
        match &self.ty {
            TlsBackendInner::None => Err(Error::NoTlsBackend),
            #[cfg(feature = "native-tls")]
            TlsBackendInner::NativeTls => Ok(TlsConnector::from(
                compio::tls::native_tls::TlsConnector::builder()
                    .request_alpns(if cfg!(feature = "http2") {
                        &["h2", "http/1.1"]
                    } else {
                        &["http/1.1"]
                    })
                    .danger_accept_invalid_certs(self.accept_invalid_certs)
                    .build()?,
            )),
            #[cfg(feature = "rustls")]
            TlsBackendInner::Rustls(config) => {
                Ok(TlsConnector::from(if let Some(config) = config.clone() {
                    config
                } else {
                    use compio::rustls::{
                        ClientConfig, DigitallySignedStruct, Error as TLSError, SignatureScheme,
                        client::danger::{
                            HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
                        },
                        pki_types::{CertificateDer, ServerName, UnixTime},
                    };
                    use rustls_platform_verifier::ConfigVerifierExt;

                    #[derive(Debug)]
                    pub(crate) struct NoVerifier;

                    impl ServerCertVerifier for NoVerifier {
                        fn verify_server_cert(
                            &self,
                            _end_entity: &CertificateDer,
                            _intermediates: &[CertificateDer],
                            _server_name: &ServerName,
                            _ocsp_response: &[u8],
                            _now: UnixTime,
                        ) -> Result<ServerCertVerified, TLSError> {
                            Ok(ServerCertVerified::assertion())
                        }

                        fn verify_tls12_signature(
                            &self,
                            _message: &[u8],
                            _cert: &CertificateDer,
                            _dss: &DigitallySignedStruct,
                        ) -> Result<HandshakeSignatureValid, TLSError> {
                            Ok(HandshakeSignatureValid::assertion())
                        }

                        fn verify_tls13_signature(
                            &self,
                            _message: &[u8],
                            _cert: &CertificateDer,
                            _dss: &DigitallySignedStruct,
                        ) -> Result<HandshakeSignatureValid, TLSError> {
                            Ok(HandshakeSignatureValid::assertion())
                        }

                        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                            vec![
                                SignatureScheme::RSA_PKCS1_SHA1,
                                SignatureScheme::ECDSA_SHA1_Legacy,
                                SignatureScheme::RSA_PKCS1_SHA256,
                                SignatureScheme::ECDSA_NISTP256_SHA256,
                                SignatureScheme::RSA_PKCS1_SHA384,
                                SignatureScheme::ECDSA_NISTP384_SHA384,
                                SignatureScheme::RSA_PKCS1_SHA512,
                                SignatureScheme::ECDSA_NISTP521_SHA512,
                                SignatureScheme::RSA_PSS_SHA256,
                                SignatureScheme::RSA_PSS_SHA384,
                                SignatureScheme::RSA_PSS_SHA512,
                                SignatureScheme::ED25519,
                                SignatureScheme::ED448,
                            ]
                        }
                    }

                    let mut config = if self.accept_invalid_certs {
                        ClientConfig::builder()
                            .dangerous()
                            .with_custom_certificate_verifier(Arc::new(NoVerifier))
                            .with_no_client_auth()
                    } else {
                        ClientConfig::with_platform_verifier()?
                    };
                    config.alpn_protocols = if cfg!(feature = "http2") {
                        vec![b"h2".into(), b"http/1.1".into()]
                    } else {
                        vec![b"http/1.1".into()]
                    };
                    config.key_log = Arc::new(compio::rustls::KeyLogFile::new());
                    Arc::new(config)
                }))
            }
        }
    }

    #[cfg(tls)]
    pub(crate) async fn connect<T>(&self, f: impl AsyncFnOnce(&TlsConnector) -> T) -> Result<T> {
        let connector = {
            let mut cache = self.connector.lock().await;
            match &*cache {
                Some(connector) => connector.clone(),
                None => {
                    let new = Arc::new(self.build_connector()?);
                    cache.get_or_insert(new).clone()
                }
            }
        };
        Ok(f(&connector).await)
    }
}
