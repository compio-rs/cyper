use std::{io, net::SocketAddr, time::Duration};

use compio::net::{TcpSocket, TcpStream};

pub async fn connect_tcp(
    server_addr: SocketAddr,
    bind_addr: Option<SocketAddr>,
    timeout: Option<Duration>,
) -> io::Result<TcpStream> {
    let fut = async move {
        if let Some(bind_addr) = bind_addr {
            let socket = if bind_addr.is_ipv4() {
                TcpSocket::new_v4().await?
            } else {
                TcpSocket::new_v6().await?
            };
            socket.bind(bind_addr).await?;
            socket.connect(server_addr).await
        } else {
            TcpStream::connect(server_addr).await
        }
    };
    if let Some(timeout) = timeout {
        compio::time::timeout(timeout, fut)
            .await
            .map_err(|_| io::ErrorKind::TimedOut.into())
            .flatten()
    } else {
        fut.await
    }
}
