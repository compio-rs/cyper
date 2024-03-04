# Cyper

[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/compio-rs/cyper/blob/master/LICENSE)
[![crates.io](https://img.shields.io/crates/v/cyper)](https://crates.io/crates/cyper)
[![docs.rs](https://img.shields.io/badge/docs.rs-cyper-latest)](https://docs.rs/cyper)
[![Check](https://github.com/compio-rs/cyper/actions/workflows/ci_check.yml/badge.svg)](https://github.com/compio-rs/cyper/actions/workflows/ci_check.yml)
[![Test](https://github.com/compio-rs/cyper/actions/workflows/ci_test.yml/badge.svg)](https://github.com/compio-rs/cyper/actions/workflows/ci_test.yml)

An HTTP library based on [compio](https://github.com/compio-rs/compio) and [hyper](https://github.com/hyperium/hyper).

## Quick start

Add `compio` and `cyper` as dependency:

```
compio = { version = "0.9.0", features = ["macros"] }
cyper = "0.1.0-beta.2"
```

Then we can start a simple HTTP request:

```rust
use cyper::Client;

#[compio::main]
async fn main() {
    let client = Client::new();
    let response = client.get("https://www.example.com/").send().await.unwrap();
    println!("{}", response.text().await.unwrap());
}
```

## Contributing

There are opportunities to contribute to Cyper at any level. It doesn't matter if
you are just getting started with Rust or are the most weathered expert, we can
use your help. If you have any question about Cyper, feel free to join our [telegram group](https://t.me/compio_rs). Before contributing, please checkout our [contributing guide](https://github.com/compio-rs/cyper/blob/master/CONTRIBUTING.md).
