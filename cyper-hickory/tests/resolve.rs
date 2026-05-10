#[cfg(feature = "__tls")]
use std::sync::Arc;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use async_trait::async_trait;
#[cfg(feature = "__tls")]
use compio::rustls::{ClientConfig, ServerConfig};
use cyper_hickory::CompioConnectionProvider;
use futures_channel::oneshot;
use hickory_net::{
    proto::{
        op::{MessageType, Metadata, OpCode},
        rr::{Name, Record, RecordType},
    },
    runtime::Time,
};
use hickory_resolver::{
    Resolver,
    config::{ResolverConfig, ServerGroup},
};
use hickory_server::{
    server::{Request, RequestHandler, ResponseHandler, ResponseInfo},
    zone_handler::{MessageResponseBuilder, UpdateRequest},
};

enum ServerType {
    Tcp,
    Udp,
    #[cfg(feature = "tls")]
    Tls(compio::rustls::ServerConfig),
    #[cfg(feature = "https")]
    Https(compio::rustls::ServerConfig, String, String),
}

const IP_RESPONSE: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));

struct DnsHandler;

#[async_trait]
impl RequestHandler for DnsHandler {
    async fn handle_request<R: ResponseHandler, T: Time>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let mut record = Record::update0(Name::from_utf8("compio.rs").unwrap(), 0, RecordType::A);
        record.data = IP_RESPONSE.into();
        let response = MessageResponseBuilder::new(&request.queries, request.edns.as_ref()).build(
            Metadata::new(request.id(), MessageType::Response, OpCode::Update),
            vec![&record],
            vec![],
            vec![],
            vec![],
        );
        response_handle.send_response(response).await.unwrap()
    }
}

async fn spawn_dns_server(ty: ServerType) -> SocketAddr {
    use tokio::net::{TcpListener, UdpSocket};

    let (tx, rx) = oneshot::channel();

    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let mut server = hickory_server::Server::new(DnsHandler);
                match ty {
                    ServerType::Tcp => {
                        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let addr = listener.local_addr().unwrap();
                        tx.send(addr).unwrap();
                        server.register_listener(listener, Duration::from_secs(5), 1024);
                    }
                    ServerType::Udp => {
                        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                        let addr = socket.local_addr().unwrap();
                        tx.send(addr).unwrap();
                        server.register_socket(socket);
                    }
                    #[cfg(feature = "tls")]
                    ServerType::Tls(config) => {
                        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let addr = listener.local_addr().unwrap();
                        tx.send(addr).unwrap();
                        server
                            .register_tls_listener_with_tls_config(
                                listener,
                                Duration::from_secs(5),
                                Arc::new(config),
                            )
                            .unwrap();
                    }
                    #[cfg(feature = "https")]
                    ServerType::Https(config, hostname, endpoint) => {
                        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let addr = listener.local_addr().unwrap();
                        tx.send(addr).unwrap();
                        server
                            .register_https_listener_with_tls_config(
                                listener,
                                Duration::from_secs(5),
                                Arc::new(config),
                                Some(hostname),
                                endpoint,
                            )
                            .unwrap();
                    }
                }
                server.block_until_done().await.unwrap();
            });
    });

    rx.await.unwrap()
}

async fn test_resolve(resolver: Resolver<CompioConnectionProvider>) {
    let ips = resolver
        .lookup_ip("compio.rs")
        .await
        .unwrap()
        .iter()
        .collect::<Vec<_>>();

    assert_eq!(&ips, &[IP_RESPONSE]);
}

fn update_port(config: &mut ResolverConfig, port: u16) {
    for srv in &mut config.name_servers {
        for conn in &mut srv.connections {
            conn.port = port;
        }
    }
}

#[compio::test]
async fn resolve_tcp() {
    let addr = spawn_dns_server(ServerType::Tcp).await;
    let group = ServerGroup {
        ips: &[addr.ip()],
        server_name: "dns.compio.rs",
        path: "/dns-query",
    };
    let mut config = ResolverConfig::from_parts(None, vec![], group.tcp().collect());
    update_port(&mut config, addr.port());
    let resolver = Resolver::builder_with_config(config, CompioConnectionProvider::default())
        .build()
        .unwrap();

    test_resolve(resolver).await;
}

#[compio::test]
async fn resolve_udp() {
    let addr = spawn_dns_server(ServerType::Udp).await;
    let group = ServerGroup {
        ips: &[addr.ip()],
        server_name: "dns.compio.rs",
        path: "/dns-query",
    };
    let mut config = ResolverConfig::from_parts(None, vec![], group.udp().collect());
    update_port(&mut config, addr.port());
    let resolver = Resolver::builder_with_config(config, CompioConnectionProvider::default())
        .build()
        .unwrap();

    test_resolve(resolver).await;
}

#[cfg(feature = "__tls")]
fn rcgen() -> (ServerConfig, ClientConfig) {
    use compio::rustls::pki_types::pem::PemObject;

    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["dns.compio.rs".into()]).unwrap();

    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            compio::rustls::pki_types::PrivateKeyDer::from_pem_slice(
                signing_key.serialize_pem().as_bytes(),
            )
            .unwrap(),
        )
        .unwrap();

    let mut store = compio::rustls::RootCertStore::empty();
    store.add(cert.der().clone()).unwrap();

    let client_config = ClientConfig::builder()
        .with_root_certificates(store)
        .with_no_client_auth();

    (server_config, client_config)
}

#[compio::test]
#[cfg(feature = "tls")]
async fn resolve_tls() {
    let (server_config, client_config) = rcgen();
    let addr = spawn_dns_server(ServerType::Tls(server_config)).await;
    let group = ServerGroup {
        ips: &[addr.ip()],
        server_name: "dns.compio.rs",
        path: "/dns-query",
    };
    let mut config = ResolverConfig::tls(&group);
    update_port(&mut config, addr.port());
    let resolver = Resolver::builder_with_config(config, CompioConnectionProvider::default())
        .with_tls_config(client_config)
        .build()
        .unwrap();

    test_resolve(resolver).await;
}

#[compio::test]
#[cfg(feature = "https")]
async fn resolve_https() {
    let (server_config, client_config) = rcgen();
    let addr = spawn_dns_server(ServerType::Https(
        server_config,
        "dns.compio.rs".to_string(),
        "/dns-query".to_string(),
    ))
    .await;
    let group = ServerGroup {
        ips: &[addr.ip()],
        server_name: "dns.compio.rs",
        path: "/dns-query",
    };
    let mut config = ResolverConfig::https(&group);
    update_port(&mut config, addr.port());
    let resolver = Resolver::builder_with_config(config, CompioConnectionProvider::default())
        .with_tls_config(client_config)
        .build()
        .unwrap();

    test_resolve(resolver).await;
}
