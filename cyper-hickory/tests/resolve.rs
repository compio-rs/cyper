use std::{collections::HashSet, net::IpAddr};

use compio::net::ToSocketAddrsAsync;
use cyper_hickory::CompioConnectionProvider;
use hickory_resolver::Resolver;

async fn test_resolve(resolver: Resolver<CompioConnectionProvider>) {
    let ips = resolver
        .lookup_ip("compio.rs")
        .await
        .unwrap()
        .iter()
        .collect::<HashSet<_>>();

    let system_answer = "compio.rs:443"
        .to_socket_addrs_async()
        .await
        .unwrap()
        .into_iter()
        .map(|addr| addr.ip())
        .collect::<HashSet<IpAddr>>();

    let intersect = ips.intersection(&system_answer).collect::<Vec<_>>();
    assert!(
        !intersect.is_empty(),
        "No common IP addresses found between resolver and system"
    );
}

#[compio::test]
async fn resolve() {
    let resolver = Resolver::builder(CompioConnectionProvider::default())
        .unwrap()
        .build()
        .unwrap();

    test_resolve(resolver).await;
}

#[compio::test]
#[cfg(feature = "tls")]
async fn resolve_tls() {
    let resolver = Resolver::builder(CompioConnectionProvider::default())
        .unwrap()
        .build()
        .unwrap();

    test_resolve(resolver).await;
}

#[compio::test]
#[cfg(feature = "https")]
async fn resolve_https() {
    let resolver = Resolver::builder(CompioConnectionProvider::default())
        .unwrap()
        .build()
        .unwrap();

    test_resolve(resolver).await;
}
