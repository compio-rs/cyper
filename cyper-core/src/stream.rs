use std::{
    borrow::Cow,
    fmt, io,
    pin::Pin,
    task::{Context, Poll, ready},
};

use compio::tls::{MaybeTlsStream, TlsStream};
use futures_util::{AsyncRead, AsyncWrite};
use send_wrapper::SendWrapper;

/// A stream wrapper for hyper.
pub struct HyperStream<S>(SendWrapper<MaybeTlsStream<S>>);

impl<S> fmt::Debug for HyperStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyperStream")
            .field("is_tls", &self.is_tls())
            .finish_non_exhaustive()
    }
}

impl<S> HyperStream<S> {
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

impl<S> HyperStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Returns the negotiated ALPN protocol.
    pub fn negotiated_alpn(&self) -> Option<Cow<'_, [u8]>> {
        self.0.negotiated_alpn()
    }
}

impl<S> hyper::rt::Read for HyperStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let uninit = buf.initialize_unfilled();
        let capacity = uninit.len();
        let res = ready!(futures_util::AsyncRead::poll_read(
            Pin::new(&mut *self.0),
            cx,
            uninit,
        ))?;
        if res > capacity {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stream reported more bytes than the read buffer can hold",
            )));
        }
        // SAFETY: `AsyncRead` receives only initialized bytes, and the byte
        // count was checked against the slice length above.
        unsafe { buf.advance(res) };
        Poll::Ready(Ok(()))
    }
}

impl<S> AsyncRead for HyperStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        futures_util::AsyncRead::poll_read(Pin::new(&mut *self.0), cx, buf)
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        futures_util::AsyncRead::poll_read_vectored(Pin::new(&mut *self.0), cx, bufs)
    }
}

impl<S> hyper::rt::Write for HyperStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        futures_util::AsyncWrite::poll_write(Pin::new(&mut *self), cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        futures_util::AsyncWrite::poll_write_vectored(Pin::new(&mut *self), cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        futures_util::AsyncWrite::poll_flush(Pin::new(&mut *self), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        futures_util::AsyncWrite::poll_close(Pin::new(&mut *self), cx)
    }
}

impl<S> AsyncWrite for HyperStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
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

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        futures_util::AsyncWrite::poll_flush(Pin::new(&mut *self.0), cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        futures_util::AsyncWrite::poll_close(Pin::new(&mut *self.0), cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct OverreportingIo;

    impl AsyncRead for OverreportingIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len() + 1))
        }
    }

    impl AsyncWrite for OverreportingIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn rejects_reads_larger_than_the_destination_buffer() {
        let mut stream = HyperStream::new_plain(OverreportingIo);
        let mut bytes = [0; 8];
        let mut read_buf = hyper::rt::ReadBuf::new(&mut bytes);
        let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());

        let result =
            hyper::rt::Read::poll_read(Pin::new(&mut stream), &mut cx, read_buf.unfilled());

        assert!(matches!(
            result,
            Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::InvalidData
        ));
    }
}
