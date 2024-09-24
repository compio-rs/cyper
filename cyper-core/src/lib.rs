//! A mid level HTTP services for [`hyper`].

#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod executor;
pub use executor::*;

#[cfg(feature = "client")]
mod connector;
#[cfg(feature = "client")]
pub use connector::*;

mod backend;
pub use backend::*;

mod stream;
pub use stream::*;
