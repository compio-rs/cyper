use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use compio::{
    quic::{
        ClientBuilder, Connection, ConnectionError, OpenStreamError, ReadError, ReadExactError,
        WriteError,
    },
    rustls::ClientConfig,
};
use hickory_net::NetError;

pub async fn connect_quic(
    server_name: std::sync::Arc<str>,
    remote_addr: SocketAddr,
    bind_addr: Option<SocketAddr>,
    mut config: ClientConfig,
    timeout: Duration,
    alpn: &[u8],
) -> Result<Connection, NetError> {
    if config.alpn_protocols.is_empty() {
        config.alpn_protocols = vec![alpn.to_vec()];
    }
    let enable_early_data = config.enable_early_data;

    let bind = bind_addr.unwrap_or_else(|| {
        if remote_addr.is_ipv4() {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
        } else {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
        }
    });

    let endpoint = ClientBuilder::new_with_rustls_client_config(config)
        .bind(bind)
        .await?;

    let mut connecting = endpoint.connect(remote_addr, &server_name, None)?;
    if enable_early_data {
        match connecting.into_0rtt() {
            Ok(conn) => return Ok(conn),
            Err(f) => connecting = f,
        }
    }

    let conn = compio::time::timeout(timeout, connecting)
        .await
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
        .map_err(ToNetError::to_net_error)?;
    Ok(conn)
}

pub trait ToNetError {
    type QuinnError;

    fn to_net_error(self) -> Self::QuinnError;
}

impl ToNetError for OpenStreamError {
    type QuinnError = NetError;

    fn to_net_error(self) -> Self::QuinnError {
        match self {
            Self::ConnectionLost(err) => err.to_net_error().into(),
            Self::StreamsExhausted => NetError::from(self.to_string()),
        }
    }
}

impl ToNetError for ConnectionError {
    type QuinnError = quinn::ConnectionError;

    fn to_net_error(self) -> Self::QuinnError {
        use quinn::ConnectionError as QuinnConnectionError;

        match self {
            Self::VersionMismatch => QuinnConnectionError::VersionMismatch,
            Self::TransportError(err) => QuinnConnectionError::TransportError(err),
            Self::ConnectionClosed(err) => QuinnConnectionError::ConnectionClosed(err),
            Self::ApplicationClosed(err) => QuinnConnectionError::ApplicationClosed(err),
            Self::Reset => QuinnConnectionError::Reset,
            Self::TimedOut => QuinnConnectionError::TimedOut,
            Self::LocallyClosed => QuinnConnectionError::LocallyClosed,
            Self::CidsExhausted => QuinnConnectionError::CidsExhausted,
        }
    }
}

impl ToNetError for WriteError {
    type QuinnError = NetError;

    fn to_net_error(self) -> Self::QuinnError {
        use quinn::WriteError as QuinnWriteError;

        let err = match self {
            Self::Stopped(code) => QuinnWriteError::Stopped(code),
            Self::ConnectionLost(err) => QuinnWriteError::ConnectionLost(err.to_net_error()),
            Self::ClosedStream => QuinnWriteError::ClosedStream,
            Self::ZeroRttRejected => QuinnWriteError::ZeroRttRejected,
            _ => return NetError::from(self.to_string()),
        };
        NetError::QuinnWriteError(err)
    }
}

impl ToNetError for ReadExactError {
    type QuinnError = quinn::ReadExactError;

    fn to_net_error(self) -> Self::QuinnError {
        use quinn::ReadExactError as QuinnReadExactError;

        match self {
            Self::FinishedEarly(len) => QuinnReadExactError::FinishedEarly(len),
            Self::ReadError(err) => QuinnReadExactError::ReadError(err.to_net_error()),
        }
    }
}

impl ToNetError for ReadError {
    type QuinnError = quinn::ReadError;

    fn to_net_error(self) -> Self::QuinnError {
        use quinn::ReadError as QuinnReadError;

        match self {
            Self::Reset(code) => QuinnReadError::Reset(code),
            Self::ConnectionLost(err) => QuinnReadError::ConnectionLost(err.to_net_error()),
            Self::ClosedStream => QuinnReadError::ClosedStream,
            Self::IllegalOrderedRead => QuinnReadError::IllegalOrderedRead,
            Self::ZeroRttRejected => QuinnReadError::ZeroRttRejected,
        }
    }
}
