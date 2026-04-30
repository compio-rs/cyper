mod decompression;
mod server;

use std::io::Write;

use decompression::Compress;
use flate2::{Compression, write::ZlibEncoder};

struct Deflate;

impl Compress for Deflate {
    fn encoding() -> &'static str {
        "deflate"
    }

    fn compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }
}

#[compio::test]
async fn deflate_response() {
    decompression::compressed_response::<Deflate>(10_000, 4096).await;
}

#[compio::test]
async fn deflate_single_byte_chunks() {
    decompression::compressed_response::<Deflate>(10, 1).await;
}

#[compio::test]
async fn test_deflate_empty_body() {
    decompression::compressed_empty_body::<Deflate>().await;
}

#[compio::test]
async fn test_accept_header_is_not_changed_if_set() {
    decompression::accept_header_is_not_changed_if_set::<Deflate>().await;
}
