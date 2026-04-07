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

use std::{borrow::Cow, collections::BTreeSet, future::Future};

use axum_core::{body::Body, extract::FromRequestParts, response::Response};
use compio::ws::tungstenite::{self as ts, protocol::WebSocketConfig};
use cyper_core::CompioStream;
use hyper::{
    Method, StatusCode, Version,
    header::{self, HeaderMap, HeaderName, HeaderValue},
    http::request::Parts,
};
use send_wrapper::SendWrapper;
use sha1::{Digest, Sha1};
// Re-export tungstenite types for convenience.
pub use ts::protocol::frame::coding::CloseCode;
pub use ts::{Bytes, Message, Utf8Bytes, protocol::CloseFrame};

use self::rejection::*;

// ===== WebSocket =====

/// A WebSocket stream.
///
/// Use [`recv`](Self::recv) and [`send`](Self::send) to communicate.
pub struct WebSocket {
    inner: compio::ws::WebSocketStream<CompioStream<hyper::upgrade::Upgraded>>,
    protocol: Option<HeaderValue>,
}

impl std::fmt::Debug for WebSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocket").finish_non_exhaustive()
    }
}

impl WebSocket {
    /// Receive another message.
    ///
    /// Returns `None` if the stream has closed.
    pub async fn recv(&mut self) -> Option<Result<Message, axum_core::Error>> {
        loop {
            match self.inner.read().await {
                Ok(msg) => {
                    // Ignore `Frame` frames as recommended by the tungstenite maintainers
                    // https://github.com/snapview/tungstenite-rs/issues/268
                    if !matches!(msg, Message::Frame(_)) {
                        return Some(Ok(msg));
                    }
                }
                Err(ts::Error::ConnectionClosed) => return None,
                Err(ts::Error::AlreadyClosed) => return None,
                Err(e) => return Some(Err(axum_core::Error::new(e))),
            }
        }
    }

    /// Send a message.
    pub async fn send(&mut self, msg: Message) -> Result<(), axum_core::Error> {
        self.inner.send(msg).await.map_err(axum_core::Error::new)
    }

    /// Close the WebSocket connection.
    pub async fn close(mut self, close_frame: Option<CloseFrame>) -> Result<(), axum_core::Error> {
        self.inner
            .close(close_frame)
            .await
            .map_err(axum_core::Error::new)
    }

    /// Return the selected WebSocket subprotocol, if one has been chosen.
    pub fn protocol(&self) -> Option<&HeaderValue> {
        self.protocol.as_ref()
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
#[must_use]
pub struct WebSocketUpgrade<F = DefaultOnFailedUpgrade> {
    config: WebSocketConfig,
    protocol: Option<HeaderValue>,
    sec_websocket_key: Option<HeaderValue>,
    on_upgrade: hyper::upgrade::OnUpgrade,
    on_failed_upgrade: F,
    sec_websocket_protocol: BTreeSet<HeaderValue>,
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
        self.protocol = protocols
            .into_iter()
            .map(Into::into)
            .find(|proto| {
                let Ok(proto) = HeaderValue::from_str(proto) else {
                    return false;
                };
                self.sec_websocket_protocol.contains(&proto)
            })
            .map(|protocol| match protocol {
                Cow::Owned(s) => HeaderValue::from_str(&s).unwrap(),
                Cow::Borrowed(s) => HeaderValue::from_static(s),
            });

        self
    }

    /// Return the WebSocket subprotocols requested by the client.
    pub fn requested_protocols(&self) -> impl Iterator<Item = &HeaderValue> {
        self.sec_websocket_protocol.iter()
    }

    /// Set the chosen WebSocket subprotocol.
    ///
    /// Another method, [`protocols()`](Self::protocols), also sets the chosen
    /// WebSocket subprotocol. If both methods are called, only the latter call
    /// takes effect.
    pub fn set_selected_protocol(&mut self, protocol: HeaderValue) {
        self.protocol = Some(protocol);
    }

    /// Return the selected WebSocket subprotocol, if one has been chosen.
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
    #[must_use = "to set up the WebSocket connection, this response must be returned"]
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

            let stream = CompioStream::new(upgraded);
            let ws = compio::ws::WebSocketStream::from_raw_socket(
                stream,
                ts::protocol::Role::Server,
                config,
            )
            .await;
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
                .is_none_or(|p| p.as_str() != "websocket")
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

        let sec_websocket_protocol = parts
            .headers
            .get_all(header::SEC_WEBSOCKET_PROTOCOL)
            .iter()
            .flat_map(|val| val.as_bytes().split(|&b| b == b','))
            .map(|proto| {
                HeaderValue::from_bytes(proto.trim_ascii())
                    .expect("substring of HeaderValue is valid HeaderValue")
            })
            .collect();

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
