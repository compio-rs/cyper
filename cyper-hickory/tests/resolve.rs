use std::{collections::HashSet, net::IpAddr};

use compio::net::ToSocketAddrsAsync;
use cyper_hickory::CompioRuntimeProvider;
use hickory_net::proto::rr::RData;
use hickory_resolver::{
    Resolver,
    config::{CLOUDFLARE, ResolverConfig},
};

#[compio::test]
async fn resolve() {
    let resolver = Resolver::builder_with_config(
        ResolverConfig::udp_and_tcp(&CLOUDFLARE),
        CompioRuntimeProvider::new_current(),
    )
    .build()
    .unwrap();

    let lookup_v4 = resolver.ipv4_lookup("compio.rs").await.unwrap();
    let lookup_v6 = resolver.ipv6_lookup("compio.rs").await.unwrap();
    let ips = lookup_v4
        .message()
        .answers
        .iter()
        .chain(lookup_v6.message().answers.iter())
        .filter_map(|record| {
            if let RData::A(ip) = record.data {
                Some(ip.0.into())
            } else if let RData::AAAA(ip) = record.data {
                Some(ip.0.into())
            } else {
                None
            }
        })
        .collect::<HashSet<IpAddr>>();

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
