use std::{
    ops::Deref,
    pin::Pin,
    task::{Context, Poll},
};

use compio::bytes::Bytes;
use hyper::body::{Frame, Incoming, SizeHint};

/// A request body.
#[derive(Debug, Default, Clone)]
pub struct Body(pub(crate) Option<Bytes>);

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

    fn size_hint(&self) -> SizeHint {
        match &self.0 {
            None => SizeHint::default(),
            Some(b) => SizeHint::with_exact(b.len() as _),
        }
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

#[derive(Debug)]
pub(crate) enum ResponseBody {
    Incoming(Incoming),
    #[cfg(feature = "http3")]
    Blob(crate::Body),
}

impl hyper::body::Body for ResponseBody {
    type Data = Bytes;
    type Error = crate::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = unsafe { self.get_unchecked_mut() };
        match this {
            Self::Incoming(b) => unsafe { Pin::new_unchecked(b) }
                .poll_frame(cx)
                .map_err(|e| e.into()),
            #[cfg(feature = "http3")]
            Self::Blob(b) => unsafe { Pin::new_unchecked(b) }.poll_frame(cx),
        }
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        match self {
            Self::Incoming(b) => b.size_hint(),
            #[cfg(feature = "http3")]
            Self::Blob(b) => b.size_hint(),
        }
    }
}
