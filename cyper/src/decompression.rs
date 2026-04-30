use std::io;

use async_stream::try_stream;
use compio::{buf::SetLen, bytes::Bytes};
use compression_codecs::{
    DecodeV2,
    core::util::{PartialBuffer, WriteBuffer},
};
use futures_util::Stream;
use http_body_util::BodyExt;
use hyper::body::Incoming;

pub struct Decoder {
    inner: Box<dyn DecodeV2 + Send + Sync>,
}

impl Decoder {
    pub fn new<D: DecodeV2 + Send + Sync + 'static>(decoder: D) -> Self {
        Self {
            inner: Box::new(decoder),
        }
    }

    pub fn new_by_name(name: &str) -> Option<Self> {
        match name {
            #[cfg(feature = "gzip")]
            "gzip" => Some(Self::new(compression_codecs::GzipDecoder::new())),
            #[cfg(feature = "deflate")]
            "deflate" => Some(Self::new(compression_codecs::DeflateDecoder::new())),
            #[cfg(feature = "brotli")]
            "br" | "brotli" => Some(Self::new(compression_codecs::BrotliDecoder::new())),
            #[cfg(feature = "zstd")]
            "zstd" => Some(Self::new(compression_codecs::ZstdDecoder::new())),
            _ => None,
        }
    }

    fn decode_impl(&mut self, data: &[u8], buffer: &mut Vec<u8>) -> io::Result<usize> {
        let mut input = PartialBuffer::new(data);
        let mut output = WriteBuffer::new_uninitialized(buffer.spare_capacity_mut());
        let eof = self.inner.decode(&mut input, &mut output)?;
        if eof {
            loop {
                if output.has_no_spare_space() {
                    let len = output.written_len();
                    buffer.reserve(4096);
                    output = WriteBuffer::new_uninitialized(buffer.spare_capacity_mut());
                    output.advance(len);
                }
                let flushed = self.inner.finish(&mut output)?;
                if flushed {
                    break;
                }
            }
        }
        unsafe {
            let output_written_len = output.written_len();
            buffer.advance(output_written_len);
        }
        Ok(input.written_len())
    }

    pub fn decode_all(&mut self, data: &[u8]) -> io::Result<Bytes> {
        let mut buffer = Vec::with_capacity(4096);
        let mut offset = 0;
        loop {
            let len = self.decode_impl(&data[offset..], &mut buffer)?;
            if len == 0 {
                break;
            }
            offset += len;
            if offset >= data.len() {
                break;
            }
        }
        Ok(Bytes::from(buffer))
    }

    pub fn decode_incoming(
        mut self,
        mut incoming: Incoming,
    ) -> impl Stream<Item = crate::Result<Bytes>> {
        try_stream! {
            while let Some(frame) = incoming.frame().await {
                let frame = frame?;
                if let Some(data) = frame.data_ref() {
                    yield self.decode_all(data)?;
                }
            }
        }
    }
}
