//! Adapters for [`nyquest_interface`].
//!
//! This support is experimental. Not all features are implemented.
//! It might break regardless of semver in future releases.

/// Register `cyper` as a backend for [`nyquest_interface`].
pub fn register() {
    nyquest_interface::register_backend(CyperBackend);
}

/// An implementation of nyquest backend.
///
/// ## Missing features
/// * `caching_behavior`
/// * `use_default_proxy`
/// * `follow_redirects`
pub struct CyperBackend;

#[cfg(feature = "nyquest-async")]
mod r#async;

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
