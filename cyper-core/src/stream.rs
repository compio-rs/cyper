use std::{
    borrow::Cow,
    io,
    mem::MaybeUninit,
    pin::Pin,
    task::{Context, Poll, ready},
};

use compio::{
    buf::{BufResult, IoBuf, IoBufMut},
    io::{AsyncRead, AsyncWrite, util::Splittable},
    tls::{MaybeTlsStream, TlsStream},
};
use send_wrapper::SendWrapper;

/// A stream wrapper for hyper.
#[derive(Debug)]
pub struct HyperStream<S: Splittable>(SendWrapper<MaybeTlsStream<S>>);

impl<S: Splittable> HyperStream<S> {
    /// Create a new [`HyperStream`] from a plain stream.
    pub fn new_plain(s: S) -> Self {
        Self(SendWrapper::new(MaybeTlsStream::new_plain(s)))
    }

    /// Create a new [`HyperStream`] from a TLS stream.
    pub fn new_tls(s: TlsStream<S>) -> Self {
        Self(SendWrapper::new(MaybeTlsStream::new_tls(s)))
    }

    /// Whether the stream is TLS-encrypted.
    pub fn is_tls(&self) -> bool {
        self.0.is_tls()
    }
}

impl<S: Splittable + 'static> HyperStream<S>
where
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
{
    /// Returns the negotiated ALPN protocol.
    pub fn negotiated_alpn(&self) -> Option<Cow<'_, [u8]>> {
        self.0.negotiated_alpn()
    }
}

impl<S: Splittable + 'static> hyper::rt::Read for HyperStream<S>
where
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let uninit = unsafe { buf.as_mut() };
        uninit.fill(MaybeUninit::new(0));
        let res = ready!(futures_util::AsyncRead::poll_read(
            Pin::new(&mut *self.0),
            cx,
            unsafe { uninit.assume_init_mut() }
        ))?;
        unsafe { buf.advance(res) };
        Poll::Ready(Ok(()))
    }
}

impl<S: Splittable + 'static> hyper::rt::Write for HyperStream<S>
where
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        futures_util::AsyncWrite::poll_write(Pin::new(&mut *self.0), cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        futures_util::AsyncWrite::poll_write_vectored(Pin::new(&mut *self.0), cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        futures_util::AsyncWrite::poll_flush(Pin::new(&mut *self.0), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        futures_util::AsyncWrite::poll_close(Pin::new(&mut *self.0), cx)
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
            std::future::poll_fn(|cx| Pin::new(&mut self.0).poll_write(cx, buf.as_init())).await;
        BufResult(result, buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        std::future::poll_fn(|cx| Pin::new(&mut self.0).poll_flush(cx)).await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        std::future::poll_fn(|cx| Pin::new(&mut self.0).poll_shutdown(cx)).await
    }
}
