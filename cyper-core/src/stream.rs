use std::{
    future::Future,
    io,
    marker::PhantomPinned,
    ops::DerefMut,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(any(feature = "native-tls", feature = "rustls"))]
use compio::tls::TlsStream;
use compio::{
    buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut},
    io::{compat::SyncStream, AsyncRead, AsyncWrite},
    net::TcpStream,
};
use hyper::Uri;
#[cfg(feature = "client")]
use hyper_util::client::legacy::connect::{Connected, Connection};
use pin_project::pin_project;
use send_wrapper::SendWrapper;

use crate::TlsBackend;

#[allow(clippy::large_enum_variant)]
enum HttpStreamInner {
    Tcp(TcpStream),
    #[cfg(any(feature = "native-tls", feature = "rustls"))]
    Tls(TlsStream<TcpStream>),
}

impl HttpStreamInner {
    pub async fn connect(uri: Uri, tls: TlsBackend) -> io::Result<Self> {
        let scheme = uri.scheme_str().unwrap_or("http");
        let host = uri.host().expect("there should be host");
        let port = uri.port_u16();
        match scheme {
            "http" => {
                let stream = TcpStream::connect((host, port.unwrap_or(80))).await?;
                // Ignore it.
                let _tls = tls;
                Ok(Self::Tcp(stream))
            }
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            "https" => {
                let stream = TcpStream::connect((host, port.unwrap_or(443))).await?;
                let connector = tls.create_connector()?;
                Ok(Self::Tls(connector.connect(host, stream).await?))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported scheme",
            )),
        }
    }

    pub fn from_tcp(s: TcpStream) -> Self {
        Self::Tcp(s)
    }

    #[cfg(any(feature = "native-tls", feature = "rustls"))]
    pub fn from_tls(s: TlsStream<TcpStream>) -> Self {
        Self::Tls(s)
    }
}

impl AsyncRead for HttpStreamInner {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            Self::Tcp(s) => s.read(buf).await,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.read(buf).await,
        }
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, buf: V) -> BufResult<usize, V> {
        match self {
            Self::Tcp(s) => s.read_vectored(buf).await,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.read_vectored(buf).await,
        }
    }
}

impl AsyncWrite for HttpStreamInner {
    async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        match self {
            Self::Tcp(s) => s.write(buf).await,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.write(buf).await,
        }
    }

    async fn write_vectored<T: IoVectoredBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        match self {
            Self::Tcp(s) => s.write_vectored(buf).await,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.write_vectored(buf).await,
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush().await,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.flush().await,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.shutdown().await,
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            Self::Tls(s) => s.shutdown().await,
        }
    }
}

/// A HTTP stream wrapper, based on compio, and exposes [`hyper::rt`]
/// interfaces.
pub struct HttpStream(HyperStream<HttpStreamInner>);

impl HttpStream {
    /// Create [`HttpStream`] with target uri and TLS backend.
    pub async fn connect(uri: Uri, tls: TlsBackend) -> io::Result<Self> {
        Ok(Self::from_inner(HttpStreamInner::connect(uri, tls).await?))
    }

    /// Create [`HttpStream`] with connected TCP stream.
    pub fn from_tcp(s: TcpStream) -> Self {
        Self::from_inner(HttpStreamInner::from_tcp(s))
    }

    /// Create [`HttpStream`] with connected TLS stream.
    #[cfg(any(feature = "native-tls", feature = "rustls"))]
    pub fn from_tls(s: TlsStream<TcpStream>) -> Self {
        Self::from_inner(HttpStreamInner::from_tls(s))
    }

    fn from_inner(s: HttpStreamInner) -> Self {
        Self(HyperStream::new(s))
    }
}

impl hyper::rt::Read for HttpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let inner = std::pin::pin!(&mut self.0);
        inner.poll_read(cx, buf)
    }
}

impl hyper::rt::Write for HttpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let inner = std::pin::pin!(&mut self.0);
        inner.poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner = std::pin::pin!(&mut self.0);
        inner.poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner = std::pin::pin!(&mut self.0);
        inner.poll_shutdown(cx)
    }
}

#[cfg(feature = "client")]
impl Connection for HttpStream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

type PinBoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// A stream wrapper for hyper.
#[pin_project]
pub struct HyperStream<S> {
    #[pin]
    inner: SendWrapper<SyncStream<S>>,
    read_future: Option<PinBoxFuture<io::Result<usize>>>,
    write_future: Option<PinBoxFuture<io::Result<usize>>>,
    shutdown_future: Option<PinBoxFuture<io::Result<()>>>,
    // This struct could not be Unpin because the futures keeps the pointer to the stream
    _p: PhantomPinned,
}

impl<S> HyperStream<S> {
    /// Create a hyper stream wrapper.
    pub fn new(s: S) -> Self {
        Self {
            inner: SendWrapper::new(SyncStream::new(s)),
            read_future: None,
            write_future: None,
            shutdown_future: None,
            _p: PhantomPinned,
        }
    }
}

macro_rules! poll_future {
    ($f:expr, $cx:expr, $e:expr) => {{
        let mut future = match $f.take() {
            Some(f) => f,
            None => Box::pin(SendWrapper::new($e)),
        };
        let f = future.as_mut();
        match f.poll($cx) {
            Poll::Pending => {
                $f.replace(future);
                return Poll::Pending;
            }
            Poll::Ready(res) => res,
        }
    }};
}

macro_rules! poll_future_would_block {
    ($f:expr, $cx:expr, $e:expr, $io:expr) => {{
        if let Some(mut f) = $f.take() {
            if f.as_mut().poll($cx).is_pending() {
                $f.replace(f);
                return Poll::Pending;
            }
        }

        match $io {
            Ok(len) => Poll::Ready(Ok(len)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                $f.replace(Box::pin(SendWrapper::new($e)));
                $cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }};
}

#[cfg(not(feature = "read_buf"))]
#[inline]
fn read_buf(reader: &mut impl io::Read, mut buf: hyper::rt::ReadBufCursor<'_>) -> io::Result<()> {
    use std::mem::MaybeUninit;

    let slice = unsafe { buf.as_mut() };
    slice.fill(MaybeUninit::new(0));
    let slice = unsafe { &mut *(slice as *mut _ as *mut [u8]) };
    let len = reader.read(slice)?;
    unsafe { buf.advance(len) };
    Ok(())
}

#[cfg(feature = "read_buf")]
#[inline]
fn read_buf(reader: &mut impl io::Read, mut buf: hyper::rt::ReadBufCursor<'_>) -> io::Result<()> {
    let slice = unsafe { buf.as_mut() };
    let len = {
        let mut borrowed_buf = io::BorrowedBuf::from(slice);
        let mut cursor = borrowed_buf.unfilled();
        reader.read_buf(cursor.reborrow())?;
        cursor.written()
    };
    unsafe { buf.advance(len) };
    Ok(())
}

impl<S: AsyncRead + Unpin + 'static> hyper::rt::Read for HyperStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let mut this = self.project();
        // Safety:
        // - The futures won't live longer than the stream.
        // - `self` is pinned.
        // - The inner stream won't be moved.
        let inner: &'static mut SyncStream<S> =
            unsafe { &mut *(this.inner.deref_mut().deref_mut() as *mut _) };

        poll_future_would_block!(
            this.read_future,
            cx,
            inner.fill_read_buf(),
            read_buf(inner, buf)
        )
    }
}

impl<S: AsyncWrite + Unpin + 'static> hyper::rt::Write for HyperStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut this = self.project();

        if this.shutdown_future.is_some() {
            debug_assert!(this.write_future.is_none());
            return Poll::Pending;
        }

        let inner: &'static mut SyncStream<S> =
            unsafe { &mut *(this.inner.deref_mut().deref_mut() as *mut _) };
        poll_future_would_block!(
            this.write_future,
            cx,
            inner.flush_write_buf(),
            io::Write::write(inner, buf)
        )
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut this = self.project();

        if this.shutdown_future.is_some() {
            debug_assert!(this.write_future.is_none());
            return Poll::Pending;
        }

        let inner: &'static mut SyncStream<S> =
            unsafe { &mut *(this.inner.deref_mut().deref_mut() as *mut _) };
        let res = poll_future!(this.write_future, cx, inner.flush_write_buf());
        Poll::Ready(res.map(|_| ()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut this = self.project();

        // Avoid shutdown on flush because the inner buffer might be passed to the
        // driver.
        if this.write_future.is_some() {
            debug_assert!(this.shutdown_future.is_none());
            return Poll::Pending;
        }

        let inner: &'static mut SyncStream<S> =
            unsafe { &mut *(this.inner.deref_mut().deref_mut() as *mut _) };
        let res = poll_future!(this.shutdown_future, cx, inner.get_mut().shutdown());
        Poll::Ready(res)
    }
}
