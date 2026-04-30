use std::io;

use async_stream::try_stream;
use compio::bytes::Bytes;
use compression_codecs::{
    DecodeV2,
    core::util::{PartialBuffer, WriteBuffer},
};
use futures_util::Stream;
use http_body_util::BodyExt;
use hyper::body::Incoming;

// flate2 requires a window-sized output buffer (~32KB).
const MIN_SPARE: usize = 32768;

pub struct Decoder<D> {
    inner: D,
}

impl<D: DecodeV2> Decoder<D> {
    pub fn new(decoder: D) -> Self {
        Self { inner: decoder }
    }

    fn decode_impl(&mut self, data: &[u8], buffer: &mut Vec<u8>) -> io::Result<usize> {
        use compio::buf::SetLen;

        if data.is_empty() {
            return Ok(0);
        }

        let mut input = PartialBuffer::new(data);
        loop {
            if buffer.spare_capacity_mut().len() < MIN_SPARE {
                buffer.reserve(MIN_SPARE);
            }
            let mut output = WriteBuffer::new_uninitialized(buffer.spare_capacity_mut());
            let result = self.inner.decode(&mut input, &mut output);
            match result {
                Ok(eof) => {
                    let output_written = output.written_len();
                    unsafe { buffer.advance(output_written) }
                    if eof {
                        loop {
                            if buffer.spare_capacity_mut().len() < MIN_SPARE {
                                buffer.reserve(MIN_SPARE);
                            }
                            let mut output =
                                WriteBuffer::new_uninitialized(buffer.spare_capacity_mut());
                            let flushed = self.inner.finish(&mut output)?;
                            let written = output.written_len();
                            unsafe { buffer.advance(written) }
                            if flushed {
                                break;
                            }
                        }
                        break;
                    }
                    if output_written == 0 {
                        break;
                    }
                }
                Err(_) if input.written_len() >= data.len() => {
                    // We've consumed all input but haven't produced output. This can happen
                    // with some codecs when the input is incomplete. We'll wait for more data
                    // to arrive before trying again.
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(input.written_len())
    }

    /// Decode a complete compressed payload.
    pub fn decode_all(&mut self, data: &[u8]) -> io::Result<Bytes> {
        let mut buffer = Vec::with_capacity(MIN_SPARE);
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
                    let bytes = self.decode_all(data)?;
                    if !bytes.is_empty() {
                        yield bytes;
                    }
                }
            }
        }
    }
}
