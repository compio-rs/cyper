mod decompression;
mod server;

use decompression::Compress;

struct Zstd;

impl Compress for Zstd {
    fn encoding() -> &'static str {
        "zstd"
    }

    fn compress(data: &[u8]) -> Vec<u8> {
        zstd_crate::encode_all(data, 3).unwrap()
    }
}

#[compio::test]
async fn zstd_response() {
    decompression::compressed_response::<Zstd>(10_000, 4096).await;
}

#[compio::test]
async fn zstd_single_byte_chunks() {
    decompression::compressed_response::<Zstd>(10, 1).await;
}

#[compio::test]
async fn test_zstd_empty_body() {
    decompression::compressed_empty_body::<Zstd>().await;
}

#[compio::test]
async fn test_accept_header_is_not_changed_if_set() {
    decompression::accept_header_is_not_changed_if_set::<Zstd>().await;
}
