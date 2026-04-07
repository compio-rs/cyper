//! WebSocket support for cyper-axum.
//!
//! This module provides a [`WebSocketUpgrade`] extractor and [`WebSocket`] type
//! that work with the compio runtime, as a replacement for `axum::extract::ws`
//! which depends on tokio.
//!
//! # Example
//!
//! ```
//! use axum::{Router, response::Response, routing::any};
//! use cyper_axum::ws::{Message, WebSocket, WebSocketUpgrade};
//!
//! let app = Router::new().route("/ws", any(handler));
//!
//! async fn handler(ws: WebSocketUpgrade) -> Response {
//!     ws.on_upgrade(handle_socket)
//! }
//!
//! async fn handle_socket(mut socket: WebSocket) {
//!     while let Some(msg) = socket.recv().await {
//!         let msg = if let Ok(msg) = msg {
//!             msg
//!         } else {
//!             return;
//!         };
//!
//!         if socket.send(msg).await.is_err() {
//!             return;
//!         }
//!     }
//! }
//! # let _: Router = app;
//! ```

use std::{
    borrow::Cow,
    future::Future,
    io,
    pin::Pin,
    task::{Poll, ready},
};

use axum_core::{body::Body, extract::FromRequestParts, response::Response};
use hyper::{
    Method, StatusCode, Version,
    header::{self, HeaderMap, HeaderName, HeaderValue},
    http::request::Parts,
    rt::{Read as _, Write as _},
    upgrade::Upgraded,
};
use send_wrapper::SendWrapper;
use sha1::{Digest, Sha1};
use tungstenite::{self as ts, protocol::WebSocketConfig};

use self::rejection::*;

// ===== UpgradedIo =====

const DEFAULT_BUF_SIZE: usize = 8 * 1024;
const MAX_BUF_SIZE: usize = 64 * 1024 * 1024;

/// Adapter that bridges `hyper::upgrade::Upgraded` (poll-based hyper::rt IO)
/// to sync `Read + Write` with internal buffers, suitable for tungstenite.
struct UpgradedIo {
    upgraded: Upgraded,
    read_buf: Vec<u8>,
    read_pos: usize,
    write_buf: Vec<u8>,
}

impl UpgradedIo {
    fn new(upgraded: Upgraded) -> Self {
        Self {
            upgraded,
            read_buf: Vec::with_capacity(DEFAULT_BUF_SIZE),
            read_pos: 0,
            write_buf: Vec::with_capacity(DEFAULT_BUF_SIZE),
        }
    }

    fn available_read(&self) -> &[u8] {
        &self.read_buf[self.read_pos..]
    }

    fn consume_read(&mut self, amt: usize) {
        self.read_pos += amt;
        if self.read_pos == self.read_buf.len() {
            self.read_buf.clear();
            self.read_pos = 0;
            // Shrink if oversized
            if self.read_buf.capacity() > DEFAULT_BUF_SIZE * 4 {
                self.read_buf.shrink_to(DEFAULT_BUF_SIZE);
            }
        }
    }

    /// Fill read buffer by polling the upgraded connection.
    async fn fill_read_buf(&mut self) -> io::Result<usize> {
        // Compact: move unconsumed data to front
        if self.read_pos > 0 {
            self.read_buf.drain(..self.read_pos);
            self.read_pos = 0;
        }

        // Ensure space
        let current_len = self.read_buf.len();
        if current_len >= MAX_BUF_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "read buffer size limit exceeded",
            ));
        }
        let needed = DEFAULT_BUF_SIZE;
        self.read_buf.reserve(needed);

        let read = futures_util::future::poll_fn(|cx| {
            // SAFETY: spare capacity is valid for writing
            let spare = self.read_buf.spare_capacity_mut();
            let mut buf = hyper::rt::ReadBuf::uninit(spare);
            let filled_before = buf.filled().len();
            ready!(Pin::new(&mut self.upgraded).poll_read(cx, buf.unfilled()))?;
            let filled_after = buf.filled().len();
            let n = filled_after - filled_before;
            Poll::Ready(Ok::<_, io::Error>(n))
        })
        .await?;

        // SAFETY: poll_read initialized `read` more bytes
        unsafe {
            self.read_buf.set_len(current_len + read);
        }
        Ok(read)
    }

    /// Flush write buffer to the upgraded connection.
    async fn flush_write_buf(&mut self) -> io::Result<usize> {
        let mut written_total = 0;
        while written_total < self.write_buf.len() {
            let written = futures_util::future::poll_fn(|cx| {
                let buf = &self.write_buf[written_total..];
                Pin::new(&mut self.upgraded).poll_write(cx, buf)
            })
            .await?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "write returned 0 bytes",
                ));
            }
            written_total += written;
        }
        self.write_buf.clear();
        if self.write_buf.capacity() > DEFAULT_BUF_SIZE * 4 {
            self.write_buf.shrink_to(DEFAULT_BUF_SIZE);
        }

        // Also flush the underlying stream
        futures_util::future::poll_fn(|cx| Pin::new(&mut self.upgraded).poll_flush(cx)).await?;

        Ok(written_total)
    }
}

impl io::Read for UpgradedIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.available_read();
        if available.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "need to fill read buffer",
            ));
        }
        let to_read = available.len().min(buf.len());
        buf[..to_read].copy_from_slice(&available[..to_read]);
        self.consume_read(to_read);
        Ok(to_read)
    }
}

impl io::Write for UpgradedIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.write_buf.len() + buf.len() > MAX_BUF_SIZE {
            let space = MAX_BUF_SIZE - self.write_buf.len();
            if space == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "write buffer full, need to flush",
                ));
            }
            self.write_buf.extend_from_slice(&buf[..space]);
            return Ok(space);
        }
        self.write_buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Intentionally no-op for tungstenite compatibility.
        // Actual flushing happens via flush_write_buf().
        Ok(())
    }
}

// ===== Message types =====

/// UTF-8 wrapper for [ts::Bytes].
///
/// An [`Utf8Bytes`] is always guaranteed to contain valid UTF-8.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Utf8Bytes(ts::Utf8Bytes);

impl Utf8Bytes {
    /// Creates from a static str.
    #[inline]
    #[must_use]
    pub const fn from_static(str: &'static str) -> Self {
        Self(ts::Utf8Bytes::from_static(str))
    }

    /// Returns as a string slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    fn into_tungstenite(self) -> ts::Utf8Bytes {
        self.0
    }
}

impl std::ops::Deref for Utf8Bytes {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl std::fmt::Display for Utf8Bytes {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<ts::Bytes> for Utf8Bytes {
    type Error = std::str::Utf8Error;

    #[inline]
    fn try_from(bytes: ts::Bytes) -> Result<Self, Self::Error> {
        Ok(Self(bytes.try_into()?))
    }
}

impl TryFrom<Vec<u8>> for Utf8Bytes {
    type Error = std::str::Utf8Error;

    #[inline]
    fn try_from(v: Vec<u8>) -> Result<Self, Self::Error> {
        Ok(Self(v.try_into()?))
    }
}

impl From<String> for Utf8Bytes {
    #[inline]
    fn from(s: String) -> Self {
        Self(s.into())
    }
}

impl From<&str> for Utf8Bytes {
    #[inline]
    fn from(s: &str) -> Self {
        Self(s.into())
    }
}

impl From<&String> for Utf8Bytes {
    #[inline]
    fn from(s: &String) -> Self {
        Self(s.into())
    }
}

impl From<Utf8Bytes> for ts::Bytes {
    #[inline]
    fn from(Utf8Bytes(bytes): Utf8Bytes) -> Self {
        bytes.into()
    }
}

impl<T> PartialEq<T> for Utf8Bytes
where
    for<'a> &'a str: PartialEq<T>,
{
    #[inline]
    fn eq(&self, other: &T) -> bool {
        self.as_str() == *other
    }
}

/// Status code used to indicate why an endpoint is closing the WebSocket
/// connection.
pub type CloseCode = u16;

/// A struct representing the close command.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CloseFrame {
    /// The reason as a code.
    pub code: CloseCode,
    /// The reason as text string.
    pub reason: Utf8Bytes,
}

/// A WebSocket message.
// This code comes from https://github.com/snapview/tungstenite-rs/blob/master/src/protocol/message.rs and is under following license:
// Copyright (c) 2017 Alexey Galakhov
// Copyright (c) 2016 Jason Housley
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.
#[derive(Debug, Eq, PartialEq, Clone)]
pub enum Message {
    /// A text WebSocket message
    Text(Utf8Bytes),
    /// A binary WebSocket message
    Binary(ts::Bytes),
    /// A ping message with the specified payload
    ///
    /// The payload here must have a length less than 125 bytes.
    Ping(ts::Bytes),
    /// A pong message with the specified payload
    ///
    /// The payload here must have a length less than 125 bytes.
    Pong(ts::Bytes),
    /// A close message with the optional close frame.
    Close(Option<CloseFrame>),
}

impl Message {
    fn into_tungstenite(self) -> ts::Message {
        match self {
            Self::Text(text) => ts::Message::Text(text.into_tungstenite()),
            Self::Binary(binary) => ts::Message::Binary(binary),
            Self::Ping(ping) => ts::Message::Ping(ping),
            Self::Pong(pong) => ts::Message::Pong(pong),
            Self::Close(Some(close)) => ts::Message::Close(Some(ts::protocol::CloseFrame {
                code: ts::protocol::frame::coding::CloseCode::from(close.code),
                reason: close.reason.into_tungstenite(),
            })),
            Self::Close(None) => ts::Message::Close(None),
        }
    }

    fn from_tungstenite(message: ts::Message) -> Option<Self> {
        match message {
            ts::Message::Text(text) => Some(Self::Text(Utf8Bytes(text))),
            ts::Message::Binary(binary) => Some(Self::Binary(binary)),
            ts::Message::Ping(ping) => Some(Self::Ping(ping)),
            ts::Message::Pong(pong) => Some(Self::Pong(pong)),
            ts::Message::Close(Some(close)) => Some(Self::Close(Some(CloseFrame {
                code: close.code.into(),
                reason: Utf8Bytes(close.reason),
            }))),
            ts::Message::Close(None) => Some(Self::Close(None)),
            // we can ignore `Frame` frames as recommended by the tungstenite maintainers
            // https://github.com/snapview/tungstenite-rs/issues/268
            ts::Message::Frame(_) => None,
        }
    }

    /// Consume the WebSocket and return it as binary data.
    pub fn into_data(self) -> ts::Bytes {
        match self {
            Self::Text(string) => ts::Bytes::from(string),
            Self::Binary(data) | Self::Ping(data) | Self::Pong(data) => data,
            Self::Close(None) => ts::Bytes::new(),
            Self::Close(Some(frame)) => ts::Bytes::from(frame.reason),
        }
    }

    /// Attempt to consume the WebSocket message and convert it to a
    /// [`Utf8Bytes`].
    pub fn into_text(self) -> Result<Utf8Bytes, axum_core::Error> {
        match self {
            Self::Text(string) => Ok(string),
            Self::Binary(data) | Self::Ping(data) | Self::Pong(data) => {
                Ok(Utf8Bytes::try_from(data).map_err(axum_core::Error::new)?)
            }
            Self::Close(None) => Ok(Utf8Bytes::default()),
            Self::Close(Some(frame)) => Ok(frame.reason),
        }
    }

    /// Attempt to get a &str from the WebSocket message,
    /// this will try to convert binary data to utf8.
    pub fn to_text(&self) -> Result<&str, axum_core::Error> {
        match *self {
            Self::Text(ref string) => Ok(string.as_str()),
            Self::Binary(ref data) | Self::Ping(ref data) | Self::Pong(ref data) => {
                Ok(std::str::from_utf8(data).map_err(axum_core::Error::new)?)
            }
            Self::Close(None) => Ok(""),
            Self::Close(Some(ref frame)) => Ok(&frame.reason),
        }
    }

    /// Create a new text WebSocket message from a stringable.
    pub fn text<S>(string: S) -> Message
    where
        S: Into<Utf8Bytes>,
    {
        Message::Text(string.into())
    }

    /// Create a new binary WebSocket message by converting to `Bytes`.
    pub fn binary<B>(bin: B) -> Message
    where
        B: Into<ts::Bytes>,
    {
        Message::Binary(bin.into())
    }
}

impl From<String> for Message {
    fn from(string: String) -> Self {
        Message::Text(string.into())
    }
}

impl<'s> From<&'s str> for Message {
    fn from(string: &'s str) -> Self {
        Message::Text(string.into())
    }
}

impl<'b> From<&'b [u8]> for Message {
    fn from(data: &'b [u8]) -> Self {
        Message::Binary(ts::Bytes::copy_from_slice(data))
    }
}

impl From<ts::Bytes> for Message {
    fn from(data: ts::Bytes) -> Self {
        Message::Binary(data)
    }
}

impl From<Vec<u8>> for Message {
    fn from(data: Vec<u8>) -> Self {
        Message::Binary(data.into())
    }
}

impl From<Message> for Vec<u8> {
    fn from(msg: Message) -> Self {
        msg.into_data().to_vec()
    }
}

// ===== WebSocket =====

/// A WebSocket stream.
///
/// Use [`recv`](Self::recv) and [`send`](Self::send) to communicate.
#[derive(Debug)]
pub struct WebSocket {
    inner: ts::WebSocket<UpgradedIo>,
    protocol: Option<HeaderValue>,
}

impl std::fmt::Debug for UpgradedIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradedIo").finish_non_exhaustive()
    }
}

impl WebSocket {
    /// Receive another message.
    ///
    /// Returns `None` if the stream has closed.
    pub async fn recv(&mut self) -> Option<Result<Message, axum_core::Error>> {
        loop {
            match self.inner.read() {
                Ok(msg) => {
                    // Flush any buffered writes (e.g. automatic pong replies)
                    let _ = self.flush().await;
                    if let Some(msg) = Message::from_tungstenite(msg) {
                        return Some(Ok(msg));
                    }
                }
                Err(ts::Error::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Need more data
                    match self.inner.get_mut().fill_read_buf().await {
                        Ok(0) => return None, // EOF
                        Ok(_) => continue,
                        Err(e) => return Some(Err(axum_core::Error::new(e))),
                    }
                }
                Err(ts::Error::ConnectionClosed) => return None,
                Err(ts::Error::AlreadyClosed) => return None,
                Err(e) => {
                    let _ = self.flush().await;
                    return Some(Err(axum_core::Error::new(e)));
                }
            }
        }
    }

    /// Send a message.
    pub async fn send(&mut self, msg: Message) -> Result<(), axum_core::Error> {
        loop {
            match self.inner.send(msg.clone().into_tungstenite()) {
                Ok(()) => break,
                Err(ts::Error::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.inner
                        .get_mut()
                        .flush_write_buf()
                        .await
                        .map_err(axum_core::Error::new)?;
                }
                Err(e) => return Err(axum_core::Error::new(e)),
            }
        }
        self.flush().await
    }

    /// Close the WebSocket connection.
    pub async fn close(mut self, close_frame: Option<CloseFrame>) -> Result<(), axum_core::Error> {
        let ts_frame = close_frame.map(|f| ts::protocol::CloseFrame {
            code: ts::protocol::frame::coding::CloseCode::from(f.code),
            reason: f.reason.into_tungstenite(),
        });
        loop {
            match self.inner.close(ts_frame.clone()) {
                Ok(()) => break,
                Err(ts::Error::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    let io = self.inner.get_mut();
                    let flushed = io.flush_write_buf().await.map_err(axum_core::Error::new)?;
                    if flushed == 0 {
                        io.fill_read_buf().await.map_err(axum_core::Error::new)?;
                    }
                }
                Err(ts::Error::ConnectionClosed) => break,
                Err(e) => return Err(axum_core::Error::new(e)),
            }
        }
        self.flush().await
    }

    /// Return the selected WebSocket subprotocol, if one has been chosen.
    pub fn protocol(&self) -> Option<&HeaderValue> {
        self.protocol.as_ref()
    }

    async fn flush(&mut self) -> Result<(), axum_core::Error> {
        loop {
            match self.inner.flush() {
                Ok(()) => break,
                Err(ts::Error::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.inner
                        .get_mut()
                        .flush_write_buf()
                        .await
                        .map_err(axum_core::Error::new)?;
                }
                Err(ts::Error::ConnectionClosed) => break,
                Err(e) => return Err(axum_core::Error::new(e)),
            }
        }
        self.inner
            .get_mut()
            .flush_write_buf()
            .await
            .map_err(axum_core::Error::new)?;
        Ok(())
    }
}

// ===== OnFailedUpgrade =====

/// What to do when a connection upgrade fails.
///
/// See [`WebSocketUpgrade::on_failed_upgrade`] for more details.
pub trait OnFailedUpgrade: 'static {
    /// Call the callback.
    fn call(self, error: axum_core::Error);
}

impl<F> OnFailedUpgrade for F
where
    F: FnOnce(axum_core::Error) + 'static,
{
    fn call(self, error: axum_core::Error) {
        self(error)
    }
}

/// The default `OnFailedUpgrade` used by `WebSocketUpgrade`.
///
/// It simply ignores the error.
#[non_exhaustive]
#[derive(Debug)]
pub struct DefaultOnFailedUpgrade;

impl OnFailedUpgrade for DefaultOnFailedUpgrade {
    #[inline]
    fn call(self, _error: axum_core::Error) {}
}

// ===== WebSocketUpgrade =====

/// Extractor for establishing WebSocket connections.
///
/// For HTTP/1.1 requests, this extractor requires the request method to be
/// `GET`; in later versions, `CONNECT` is used instead. To support both, it
/// should be used with [`any`](axum::routing::any).
#[cfg_attr(docsrs, doc(cfg(feature = "ws")))]
#[must_use]
pub struct WebSocketUpgrade<F = DefaultOnFailedUpgrade> {
    config: WebSocketConfig,
    protocol: Option<HeaderValue>,
    sec_websocket_key: Option<HeaderValue>,
    on_upgrade: hyper::upgrade::OnUpgrade,
    on_failed_upgrade: F,
    sec_websocket_protocol: Option<HeaderValue>,
}

impl<F> std::fmt::Debug for WebSocketUpgrade<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketUpgrade")
            .field("config", &self.config)
            .field("protocol", &self.protocol)
            .field("sec_websocket_key", &self.sec_websocket_key)
            .field("sec_websocket_protocol", &self.sec_websocket_protocol)
            .finish_non_exhaustive()
    }
}

impl<F> WebSocketUpgrade<F> {
    /// Read buffer capacity. The default value is 128KiB.
    pub fn read_buffer_size(mut self, size: usize) -> Self {
        self.config.read_buffer_size = size;
        self
    }

    /// The target minimum size of the write buffer to reach before writing the
    /// data to the underlying stream.
    ///
    /// The default value is 128 KiB.
    pub fn write_buffer_size(mut self, size: usize) -> Self {
        self.config.write_buffer_size = size;
        self
    }

    /// The max size of the write buffer in bytes. Setting this can provide
    /// backpressure in the case the write buffer is filling up due to write
    /// errors.
    ///
    /// The default value is unlimited.
    pub fn max_write_buffer_size(mut self, max: usize) -> Self {
        self.config.max_write_buffer_size = max;
        self
    }

    /// Set the maximum message size (defaults to 64 megabytes)
    pub fn max_message_size(mut self, max: usize) -> Self {
        self.config.max_message_size = Some(max);
        self
    }

    /// Set the maximum frame size (defaults to 16 megabytes)
    pub fn max_frame_size(mut self, max: usize) -> Self {
        self.config.max_frame_size = Some(max);
        self
    }

    /// Allow server to accept unmasked frames (defaults to false)
    pub fn accept_unmasked_frames(mut self, accept: bool) -> Self {
        self.config.accept_unmasked_frames = accept;
        self
    }

    /// Set the known protocols.
    ///
    /// If the protocol name specified by `Sec-WebSocket-Protocol` header
    /// to match any of them, the upgrade response will include
    /// `Sec-WebSocket-Protocol` header and return the protocol name.
    ///
    /// The protocols should be listed in decreasing order of preference: if the
    /// client offers multiple protocols that the server could support, the
    /// server will pick the first one in this list.
    pub fn protocols<I>(mut self, protocols: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<Cow<'static, str>>,
    {
        if let Some(req_protocols) = self
            .sec_websocket_protocol
            .as_ref()
            .and_then(|p| p.to_str().ok())
        {
            self.protocol = protocols
                .into_iter()
                .map(Into::into)
                .find(|protocol| {
                    req_protocols
                        .split(',')
                        .any(|req_protocol| req_protocol.trim() == protocol.as_ref())
                })
                .map(|protocol| match protocol {
                    Cow::Owned(s) => HeaderValue::from_str(&s).unwrap(),
                    Cow::Borrowed(s) => HeaderValue::from_static(s),
                });
        }

        self
    }

    /// Return the selected protocol after calling
    /// [`protocols`](Self::protocols).
    pub fn selected_protocol(&self) -> Option<&HeaderValue> {
        self.protocol.as_ref()
    }

    /// Set a callback to be called if upgrading the connection fails.
    pub fn on_failed_upgrade<C>(self, callback: C) -> WebSocketUpgrade<C>
    where
        C: OnFailedUpgrade,
    {
        WebSocketUpgrade {
            config: self.config,
            protocol: self.protocol,
            sec_websocket_key: self.sec_websocket_key,
            on_upgrade: self.on_upgrade,
            on_failed_upgrade: callback,
            sec_websocket_protocol: self.sec_websocket_protocol,
        }
    }

    /// Finalize upgrading the connection and call the provided callback with
    /// the stream.
    pub fn on_upgrade<C, Fut>(self, callback: C) -> Response
    where
        C: FnOnce(WebSocket) -> Fut + 'static,
        Fut: Future<Output = ()> + 'static,
        F: OnFailedUpgrade,
    {
        let on_upgrade = self.on_upgrade;
        let config = self.config;
        let on_failed_upgrade = self.on_failed_upgrade;
        let protocol = self.protocol.clone();

        compio::runtime::spawn(SendWrapper::new(async move {
            let upgraded = match on_upgrade.await {
                Ok(upgraded) => upgraded,
                Err(err) => {
                    on_failed_upgrade.call(axum_core::Error::new(err));
                    return;
                }
            };

            let io = UpgradedIo::new(upgraded);
            let ws = ts::WebSocket::from_raw_socket(io, ts::protocol::Role::Server, Some(config));
            let socket = WebSocket {
                inner: ws,
                protocol,
            };
            callback(socket).await;
        }))
        .detach();

        let mut response = if let Some(sec_websocket_key) = &self.sec_websocket_key {
            #[allow(clippy::declare_interior_mutable_const)]
            const UPGRADE: HeaderValue = HeaderValue::from_static("upgrade");
            #[allow(clippy::declare_interior_mutable_const)]
            const WEBSOCKET: HeaderValue = HeaderValue::from_static("websocket");

            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header(header::CONNECTION, UPGRADE)
                .header(header::UPGRADE, WEBSOCKET)
                .header(
                    header::SEC_WEBSOCKET_ACCEPT,
                    sign(sec_websocket_key.as_bytes()),
                )
                .body(Body::empty())
                .unwrap()
        } else {
            Response::new(Body::empty())
        };

        if let Some(protocol) = self.protocol {
            response
                .headers_mut()
                .insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
        }

        response
    }
}

// ===== FromRequestParts =====

impl<S> FromRequestParts<S> for WebSocketUpgrade<DefaultOnFailedUpgrade>
where
    S: Send + Sync,
{
    type Rejection = WebSocketUpgradeRejection;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let sec_websocket_key = if parts.version <= Version::HTTP_11 {
            if parts.method != Method::GET {
                return Err(MethodNotGet.into());
            }

            if !header_contains(&parts.headers, header::CONNECTION, "upgrade") {
                return Err(InvalidConnectionHeader.into());
            }

            if !header_eq(&parts.headers, header::UPGRADE, "websocket") {
                return Err(InvalidUpgradeHeader.into());
            }

            Some(
                parts
                    .headers
                    .get(header::SEC_WEBSOCKET_KEY)
                    .ok_or(WebSocketKeyHeaderMissing)?
                    .clone(),
            )
        } else {
            if parts.method != Method::CONNECT {
                return Err(MethodNotConnect.into());
            }

            #[cfg(feature = "http2")]
            if parts
                .extensions
                .get::<hyper::ext::Protocol>()
                .map_or(true, |p| p.as_str() != "websocket")
            {
                return Err(InvalidProtocolPseudoheader.into());
            }

            None
        };

        if !header_eq(&parts.headers, header::SEC_WEBSOCKET_VERSION, "13") {
            return Err(InvalidWebSocketVersionHeader.into());
        }

        let on_upgrade = parts
            .extensions
            .remove::<hyper::upgrade::OnUpgrade>()
            .ok_or(ConnectionNotUpgradable)?;

        let sec_websocket_protocol = parts.headers.get(header::SEC_WEBSOCKET_PROTOCOL).cloned();

        Ok(Self {
            config: Default::default(),
            protocol: None,
            sec_websocket_key,
            on_upgrade,
            sec_websocket_protocol,
            on_failed_upgrade: DefaultOnFailedUpgrade,
        })
    }
}

// ===== Helpers =====

fn header_eq(headers: &HeaderMap, key: HeaderName, value: &'static str) -> bool {
    if let Some(header) = headers.get(&key) {
        header.as_bytes().eq_ignore_ascii_case(value.as_bytes())
    } else {
        false
    }
}

fn header_contains(headers: &HeaderMap, key: HeaderName, value: &'static str) -> bool {
    let header = if let Some(header) = headers.get(&key) {
        header
    } else {
        return false;
    };

    if let Ok(header) = std::str::from_utf8(header.as_bytes()) {
        header.to_ascii_lowercase().contains(value)
    } else {
        false
    }
}

fn sign(key: &[u8]) -> HeaderValue {
    use base64::engine::Engine as _;

    let mut sha1 = Sha1::default();
    sha1.update(key);
    sha1.update(&b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"[..]);
    let b64 = ts::Bytes::from(base64::engine::general_purpose::STANDARD.encode(sha1.finalize()));
    HeaderValue::from_maybe_shared(b64).expect("base64 is a valid value")
}

// ===== Rejection types =====

pub mod rejection {
    //! WebSocket specific rejections.

    use axum_core::{
        __composite_rejection as composite_rejection, __define_rejection as define_rejection,
    };
    use hyper::http;

    define_rejection! {
        #[status = METHOD_NOT_ALLOWED]
        #[body = "Request method must be `GET`"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct MethodNotGet;
    }

    define_rejection! {
        #[status = METHOD_NOT_ALLOWED]
        #[body = "Request method must be `CONNECT`"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct MethodNotConnect;
    }

    define_rejection! {
        #[status = BAD_REQUEST]
        #[body = "Connection header did not include 'upgrade'"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct InvalidConnectionHeader;
    }

    define_rejection! {
        #[status = BAD_REQUEST]
        #[body = "`Upgrade` header did not include 'websocket'"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct InvalidUpgradeHeader;
    }

    define_rejection! {
        #[status = BAD_REQUEST]
        #[body = "`:protocol` pseudo-header did not include 'websocket'"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct InvalidProtocolPseudoheader;
    }

    define_rejection! {
        #[status = BAD_REQUEST]
        #[body = "`Sec-WebSocket-Version` header did not include '13'"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct InvalidWebSocketVersionHeader;
    }

    define_rejection! {
        #[status = BAD_REQUEST]
        #[body = "`Sec-WebSocket-Key` header missing"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct WebSocketKeyHeaderMissing;
    }

    define_rejection! {
        #[status = UPGRADE_REQUIRED]
        #[body = "WebSocket request couldn't be upgraded since no upgrade state was present"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct ConnectionNotUpgradable;
    }

    composite_rejection! {
        /// Rejection used for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        ///
        /// Contains one variant for each way the
        /// [`WebSocketUpgrade`](super::WebSocketUpgrade) extractor can fail.
        pub enum WebSocketUpgradeRejection {
            MethodNotGet,
            MethodNotConnect,
            InvalidConnectionHeader,
            InvalidUpgradeHeader,
            InvalidProtocolPseudoheader,
            InvalidWebSocketVersionHeader,
            WebSocketKeyHeaderMissing,
            ConnectionNotUpgradable,
        }
    }
}

// ===== Close codes =====

pub mod close_code {
    //! Constants for [`CloseCode`]s.
    //!
    //! [`CloseCode`]: super::CloseCode

    /// Indicates a normal closure, meaning that the purpose for which the
    /// connection was established has been fulfilled.
    pub const NORMAL: u16 = 1000;
    /// Indicates that an endpoint is "going away", such as a server going down
    /// or a browser having navigated away from a page.
    pub const AWAY: u16 = 1001;
    /// Indicates that an endpoint is terminating the connection due to a
    /// protocol error.
    pub const PROTOCOL: u16 = 1002;
    /// Indicates that an endpoint is terminating the connection because it has
    /// received a type of data that it cannot accept.
    pub const UNSUPPORTED: u16 = 1003;
    /// Indicates that no status code was included in a closing frame.
    pub const STATUS: u16 = 1005;
    /// Indicates an abnormal closure.
    pub const ABNORMAL: u16 = 1006;
    /// Indicates that an endpoint is terminating the connection because it has
    /// received data within a message that was not consistent with the type of
    /// the message.
    pub const INVALID: u16 = 1007;
    /// Indicates that an endpoint is terminating the connection because it has
    /// received a message that violates its policy.
    pub const POLICY: u16 = 1008;
    /// Indicates that an endpoint is terminating the connection because it has
    /// received a message that is too big for it to process.
    pub const SIZE: u16 = 1009;
    /// Indicates that an endpoint (client) is terminating the connection
    /// because the server did not respond to extension negotiation
    /// correctly.
    pub const EXTENSION: u16 = 1010;
    /// Indicates that a server is terminating the connection because it
    /// encountered an unexpected condition that prevented it from fulfilling
    /// the request.
    pub const ERROR: u16 = 1011;
    /// Indicates that the server is restarting.
    pub const RESTART: u16 = 1012;
    /// Indicates that the server is overloaded and the client should either
    /// connect to a different IP (when multiple targets exist), or reconnect to
    /// the same IP when a user has performed an action.
    pub const AGAIN: u16 = 1013;
}
