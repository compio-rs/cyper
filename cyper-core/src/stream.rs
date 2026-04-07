use std::{
    io,
    mem::MaybeUninit,
    ops::DerefMut,
    pin::Pin,
    task::{Context, Poll, ready},
};

use compio::{
    buf::{BufResult, IoBuf, IoBufMut},
    io::{AsyncRead, AsyncWrite, compat::AsyncStream},
};
use send_wrapper::SendWrapper;

/// A stream wrapper for hyper.
pub struct HyperStream<S>(SendWrapper<AsyncStream<S>>);

impl<S> HyperStream<S> {
    /// Create a hyper stream wrapper.
    pub fn new(s: S) -> Self {
        Self(SendWrapper::new(AsyncStream::new(s)))
    }

    /// Get the reference of the inner stream.
    pub fn get_ref(&self) -> &S {
        self.0.get_ref()
    }
}

impl<S> std::fmt::Debug for HyperStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HyperStream").finish_non_exhaustive()
    }
}

impl<S: AsyncRead + Unpin + 'static> hyper::rt::Read for HyperStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let stream = unsafe { self.map_unchecked_mut(|this| this.0.deref_mut()) };
        let slice = unsafe { buf.as_mut() };
        let len = ready!(stream.poll_read_uninit(cx, slice))?;
        unsafe { buf.advance(len) };
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin + 'static> hyper::rt::Write for HyperStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let stream = unsafe { self.map_unchecked_mut(|this| this.0.deref_mut()) };
        futures_util::AsyncWrite::poll_write(stream, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let stream = unsafe { self.map_unchecked_mut(|this| this.0.deref_mut()) };
        futures_util::AsyncWrite::poll_flush(stream, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let stream = unsafe { self.map_unchecked_mut(|this| this.0.deref_mut()) };
        futures_util::AsyncWrite::poll_close(stream, cx)
    }
}

/// A wrapper that bridges hyper's poll-based IO (`hyper::rt::Read + Write`)
/// to compio's ownership-based async IO (`AsyncRead + AsyncWrite`).
///
/// This is the reverse of [`HyperStream`]: where `HyperStream` wraps a compio
/// stream for use with hyper, `CompioStream` wraps a hyper stream for use with
/// compio.
pub struct CompioStream<S>(S);

impl<S> CompioStream<S> {
    /// Create a compio stream wrapper from a hyper-compatible stream.
    pub fn new(s: S) -> Self {
        Self(s)
    }

    /// Get a reference to the inner stream.
    pub fn get_ref(&self) -> &S {
        &self.0
    }

    /// Get a mutable reference to the inner stream.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.0
    }

    /// Consume the wrapper, returning the inner stream.
    pub fn into_inner(self) -> S {
        self.0
    }
}

impl<S: std::fmt::Debug> std::fmt::Debug for CompioStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompioStream")
            .field("inner", &self.0)
            .finish()
    }
}

impl<S: hyper::rt::Read + Unpin> AsyncRead for CompioStream<S> {
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        let result = std::future::poll_fn(|cx| {
            let uninit: &mut [MaybeUninit<u8>] = buf.as_uninit();
            let mut read_buf = hyper::rt::ReadBuf::uninit(uninit);
            ready!(Pin::new(&mut self.0).poll_read(cx, read_buf.unfilled()))?;
            Poll::Ready(Ok::<_, io::Error>(read_buf.filled().len()))
        })
        .await;
        match result {
            Ok(n) => {
                // SAFETY: poll_read initialized `n` bytes from the beginning.
                unsafe { buf.set_len(n) };
                BufResult(Ok(n), buf)
            }
            Err(e) => BufResult(Err(e), buf),
        }
    }
}

impl<S: hyper::rt::Write + Unpin> AsyncWrite for CompioStream<S> {
    async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        let result =
            std::future::poll_fn(|cx| Pin::new(&mut self.0).poll_write(cx, buf.as_init()))
                .await;
        BufResult(result, buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        std::future::poll_fn(|cx| Pin::new(&mut self.0).poll_flush(cx)).await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        std::future::poll_fn(|cx| Pin::new(&mut self.0).poll_shutdown(cx)).await
    }
}
