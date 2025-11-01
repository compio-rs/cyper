use std::{io::Read, rc::Rc};

use compio::{bytes::Bytes, runtime::Runtime};
use futures_util::StreamExt;
use nyquest_interface::{
    Body, Request, Result,
    blocking::{BlockingBackend, BlockingClient, BlockingResponse, BoxedStream},
    client::ClientOptions,
};
use send_wrapper::SendWrapper;

use super::{CyperBackend, CyperClient, CyperResponse, to_io_error};

impl BlockingBackend for CyperBackend {
    type BlockingClient = CyperBlockingClient;

    fn create_blocking_client(&self, options: ClientOptions) -> Result<Self::BlockingClient> {
        let client = self.create_client(options)?;
        let runtime = Rc::new(Runtime::new()?);
        Ok(CyperBlockingClient {
            client,
            runtime: SendWrapper::new(runtime),
        })
    }
}

#[derive(Clone)]
pub struct CyperBlockingClient {
    client: CyperClient,
    runtime: SendWrapper<Rc<Runtime>>,
}

impl BlockingClient for CyperBlockingClient {
    type Response = CyperBlockingResponse;

    fn request(&self, req: Request<BoxedStream>) -> Result<Self::Response> {
        let resp = self.runtime.block_on(self.client.request(Request {
            method: req.method,
            relative_uri: req.relative_uri,
            additional_headers: req.additional_headers,
            body: req.body.map(|body| match body {
                Body::Stream {
                    stream,
                    content_type,
                } => Body::Stream {
                    stream: futures_util::stream::iter(WrapBoxedStream(stream)),
                    content_type,
                },
                Body::Bytes {
                    content,
                    content_type,
                } => Body::Bytes {
                    content,
                    content_type,
                },
                Body::Form { fields } => Body::Form { fields },
            }),
        }))?;
        Ok(CyperBlockingResponse {
            resp,
            runtime: self.runtime.clone(),
        })
    }
}

struct WrapBoxedStream(BoxedStream);

impl Iterator for WrapBoxedStream {
    type Item = crate::Result<Bytes>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut buffer = [0u8; 1024];
        let res = self.0.read(&mut buffer);
        match res {
            Ok(0) => None,
            Ok(n) => {
                let bytes = Bytes::copy_from_slice(&buffer[..n]);
                Some(Ok(bytes))
            }
            Err(e) => Some(Err(e.into())),
        }
    }
}

pub struct CyperBlockingResponse {
    resp: CyperResponse,
    runtime: SendWrapper<Rc<Runtime>>,
}

impl Read for CyperBlockingResponse {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let res = self.runtime.block_on(self.resp.resp.next());
        match res {
            Some(Ok(bytes)) => {
                let len = bytes.len().min(buf.len());
                buf[..len].copy_from_slice(&bytes[..len]);
                Ok(len)
            }
            Some(Err(e)) => Err(to_io_error(e)),
            None => Ok(0),
        }
    }
}

impl BlockingResponse for CyperBlockingResponse {
    fn status(&self) -> u16 {
        self.resp.status()
    }

    fn content_length(&self) -> Option<u64> {
        self.resp.content_length()
    }

    fn get_header(&self, header: &str) -> Result<Vec<String>> {
        self.resp.get_header(header)
    }

    fn text(&mut self) -> Result<String> {
        BlockingResponse::bytes(self).map(|b| String::from_utf8_lossy(&b).to_string())
    }

    fn bytes(&mut self) -> Result<Vec<u8>> {
        self.runtime.block_on(self.resp.bytes_impl())
    }
}
