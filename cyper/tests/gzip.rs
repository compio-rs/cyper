mod decompression;
mod server;

use std::io::Write;

use decompression::Compress;
use flate2::{Compression, write::GzEncoder};

struct Gzip;

impl Compress for Gzip {
    fn encoding() -> &'static str {
        "gzip"
    }

    fn compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }
}

#[compio::test]
async fn gzip_response() {
    decompression::compressed_response::<Gzip>(10_000, 4096).await;
}

#[compio::test]
async fn gzip_single_byte_chunks() {
    decompression::compressed_response::<Gzip>(10, 1).await;
}

#[compio::test]
async fn test_gzip_empty_body() {
    decompression::compressed_empty_body::<Gzip>().await;
}

#[compio::test]
async fn test_accept_header_is_not_changed_if_set() {
    decompression::accept_header_is_not_changed_if_set::<Gzip>().await;
}
