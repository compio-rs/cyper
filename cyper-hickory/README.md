# cyper-hickory

Adapter for `hickory-resolver`.

## Features

* `tls`: DoT: DNS over TLS
* `https`: DoH: DNS over HTTPS
* `quic`: DoQ: DNS over QUIC
* `h3`: DoH3: DNS over HTTPS 3

## Usage

```
cargo add cyper-hickory
cargo add hickory-resolver --features system-config --no-default-features
cargo add compio --features macros
```

Use the `CompioConnectionProvider` as the provider of the resolver:
```rust
use cyper_hickory::CompioConnectionProvider;
use hickory_resolver::Resolver;

#[compio::main]
async fn main() {
    let resolver = Resolver::builder(CompioConnectionProvider::default())
        .unwrap()
        .build()
        .unwrap();
    let ips = resolver
        .lookup_ip("compio.rs")
        .await
        .unwrap()
        .iter()
        .collect::<Vec<_>>();
    println!("{:?}", ips);
}
```
