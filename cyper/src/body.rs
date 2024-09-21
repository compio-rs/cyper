use std::{
    ops::Deref,
    pin::Pin,
    task::{Context, Poll},
};

use compio::bytes::Bytes;
use hyper::body::Frame;

/// A request body.
#[derive(Debug, Default, Clone)]
pub struct Body(Option<Bytes>);

impl Body {
    /// Create an empty request body.
    pub const fn empty() -> Self {
        Self(None)
    }
}

impl hyper::body::Body for Body {
    type Data = Bytes;
    type Error = crate::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(self.0.take().map(|buf| Ok(Frame::data(buf))))
    }
}

impl From<String> for Body {
    fn from(value: String) -> Self {
        Self(Some(Bytes::from(value.into_bytes())))
    }
}

impl From<&'static str> for Body {
    fn from(value: &'static str) -> Self {
        Self(Some(Bytes::from_static(value.as_bytes())))
    }
}

impl From<Vec<u8>> for Body {
    fn from(value: Vec<u8>) -> Self {
        Self(Some(Bytes::from(value)))
    }
}

impl From<&'static [u8]> for Body {
    fn from(value: &'static [u8]) -> Self {
        Self(Some(Bytes::from_static(value)))
    }
}

impl Deref for Body {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match &self.0 {
            Some(bytes) => bytes,
            None => &[],
        }
    }
}
