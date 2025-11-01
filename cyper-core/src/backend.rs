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
    NativeTls {
        /// Accept invalid certificates.
        accept_invalid_certs: bool,
    },
    /// Use [`rustls`] as TLS backend.
    #[cfg(feature = "rustls")]
    Rustls {
        /// Optional custom Rustls client configuration.
        config: Option<std::sync::Arc<compio::tls::rustls::ClientConfig>>,
        /// Accept invalid certificates. Only has effect when `config` is
        /// `None`.
        accept_invalid_certs: bool,
    },
}

#[allow(clippy::derivable_impls)]
impl Default for TlsBackend {
    fn default() -> Self {
        cfg_if::cfg_if! {
            if #[cfg(feature = "native-tls")] {
                Self::NativeTls { accept_invalid_certs: false }
            } else if #[cfg(feature = "rustls")] {
                Self::Rustls { config: None, accept_invalid_certs: false }
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
            Self::NativeTls {
                accept_invalid_certs,
            } => Ok(TlsConnector::from(
                compio::tls::native_tls::TlsConnector::builder()
                    .request_alpns(if cfg!(feature = "http2") {
                        &["h2", "http/1.1"]
                    } else {
                        &["http/1.1"]
                    })
                    .danger_accept_invalid_certs(*accept_invalid_certs)
                    .build()
                    .map_err(io::Error::other)?,
            )),
            #[cfg(feature = "rustls")]
            Self::Rustls {
                config,
                accept_invalid_certs,
            } => Ok(TlsConnector::from(if let Some(config) = config.clone() {
                config
            } else {
                use std::sync::Arc;

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

                let mut config = if *accept_invalid_certs {
                    ClientConfig::builder()
                        .dangerous()
                        .with_custom_certificate_verifier(Arc::new(NoVerifier))
                        .with_no_client_auth()
                } else {
                    ClientConfig::with_platform_verifier().map_err(io::Error::other)?
                };
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
