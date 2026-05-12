use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use compio::{bytes::Bytes, quic::Connection, rustls::ClientConfig};
use compio_log::debug;
use futures_util::Stream;
use hickory_net::{
    NetError,
    proto::{
        ProtoError,
        op::{DnsRequest, DnsResponse, Message},
    },
    quic::DoqErrorCode,
    xfer::{DnsExchange, DnsRequestSender, DnsResponseStream},
};
use send_wrapper::SendWrapper;

use crate::CompioRuntimeProvider;

const DOQ_ALPN: &[u8] = b"doq";

pub async fn connect_quic(
    server_name: Arc<str>,
    remote_addr: SocketAddr,
    bind_addr: Option<SocketAddr>,
    config: ClientConfig,
    timeout: Duration,
) -> Result<DnsExchange<CompioRuntimeProvider>, NetError> {
    let conn = crate::connect_quic(
        server_name,
        remote_addr,
        bind_addr,
        config,
        timeout,
        DOQ_ALPN,
    )
    .await?;

    let stream = CompioQuicClientStream::new(conn);
    let (exchange, bg) = DnsExchange::from_stream(stream);
    compio::runtime::spawn(bg).detach();
    Ok(exchange)
}

struct CompioQuicClientStream {
    conn: SendWrapper<Connection>,
    is_shutdown: bool,
}

impl CompioQuicClientStream {
    fn new(conn: Connection) -> Self {
        Self {
            conn: SendWrapper::new(conn),
            is_shutdown: false,
        }
    }

    async fn inner_send(
        conn: SendWrapper<Connection>,
        request: DnsRequest,
    ) -> Result<DnsResponse, NetError> {
        let (send, recv) = conn
            .open_bi()
            .map_err(|e| NetError::from(format!("open_bi error: {e}")))?;

        let mut send = send.into_compat();
        let mut recv = recv.into_compat();

        let mut message = request.into_parts().0;
        message.metadata.id = 0;

        let bytes = Bytes::from(message.to_vec()?);
        let len = u16::try_from(bytes.len())
            .map_err(|_| NetError::from(ProtoError::MaxBufferSizeExceeded(bytes.len())))?;

        let len_bytes = Bytes::from(len.to_be_bytes().to_vec());

        send.write_all_chunks(&mut [len_bytes, bytes])
            .await
            .map_err(|e| NetError::from(format!("quic write error: {e}")))?;

        send.finish()
            .map_err(|e| NetError::from(format!("quic finish error: {e}")))?;

        let mut len_buf = [0u8; 2];
        recv.read_exact(&mut len_buf[..])
            .await
            .map_err(|e| NetError::from(format!("quic read length error: {e}")))?;
        let response_len = u16::from_be_bytes(len_buf) as usize;

        let mut msg_buf = vec![0u8; response_len];
        recv.read_exact(&mut msg_buf[..])
            .await
            .map_err(|e| NetError::from(format!("quic read message error: {e}")))?;

        let message = Message::from_vec(&msg_buf)?;
        if message.id != 0 {
            if let Err(_e) = send.reset(DoqErrorCode::ProtocolError.into()) {
                debug!("failed to reset stream: {_e:?}");
            }
            return Err(NetError::QuicMessageIdNot0(message.id));
        }

        Ok(DnsResponse::from_buffer(msg_buf)?)
    }
}

impl DnsRequestSender for CompioQuicClientStream {
    fn send_message(&mut self, request: DnsRequest) -> DnsResponseStream {
        if self.is_shutdown {
            panic!("can not send messages after stream is shutdown")
        }

        Box::pin(SendWrapper::new(Self::inner_send(
            self.conn.clone(),
            request,
        )))
        .into()
    }

    fn shutdown(&mut self) {
        self.is_shutdown = true;
        self.conn.close(DoqErrorCode::NoError.into(), b"shutdown");
    }

    fn is_shutdown(&self) -> bool {
        self.is_shutdown
    }
}

impl Stream for CompioQuicClientStream {
    type Item = Result<(), NetError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_shutdown {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Ok(())))
        }
    }
}
