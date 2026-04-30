mod decompression;
mod server;

use std::io::Read;

use decompression::Compress;

struct Brotli;

impl Compress for Brotli {
    fn encoding() -> &'static str {
        "br"
    }

    fn compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = brotli_crate::CompressorReader::new(data, 4096, 5, 20);
        let mut compressed = Vec::new();
        encoder.read_to_end(&mut compressed).unwrap();
        compressed
    }
}

#[compio::test]
async fn brotli_response() {
    decompression::compressed_response::<Brotli>(10_000, 4096).await;
}

#[compio::test]
async fn brotli_single_byte_chunks() {
    decompression::compressed_response::<Brotli>(10, 1).await;
}

#[compio::test]
async fn test_brotli_empty_body() {
    decompression::compressed_empty_body::<Brotli>().await;
}

#[compio::test]
async fn test_accept_header_is_not_changed_if_set() {
    decompression::accept_header_is_not_changed_if_set::<Brotli>().await;
}
