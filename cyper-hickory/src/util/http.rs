use std::str::FromStr;

use hickory_net::{NetError, proto::op::DnsResponse};
use http::uri::{Authority, Parts, PathAndQuery, Scheme, Uri};

pub const MIME_APPLICATION_DNS: &str = "application/dns-message";

pub fn build_request(
    server_name: &str,
    path: &str,
    len: usize,
) -> Result<http::Request<()>, NetError> {
    let mut parts = Parts::default();
    parts.path_and_query = Some(
        PathAndQuery::from_str(path).map_err(|e| NetError::from(format!("invalid path: {e:?}")))?,
    );
    parts.scheme = Some(Scheme::HTTPS);
    parts.authority = Some(
        Authority::from_str(server_name)
            .map_err(|e| NetError::from(format!("invalid authority: {e:?}")))?,
    );

    let url =
        Uri::from_parts(parts).map_err(|e| NetError::from(format!("uri parse error: {e:?}")))?;

    let request = http::Request::builder()
        .method("POST")
        .uri(url)
        .header(http::header::CONTENT_TYPE, MIME_APPLICATION_DNS)
        .header(http::header::ACCEPT, MIME_APPLICATION_DNS)
        .header(http::header::CONTENT_LENGTH, len)
        .body(())
        .map_err(|e| NetError::from(format!("build request error: {e:?}")))?;

    Ok(request)
}

pub fn get_content_length(headers: &http::HeaderMap) -> Result<Option<usize>, NetError> {
    headers
        .get(http::header::CONTENT_LENGTH)
        .map(|v| v.to_str())
        .transpose()
        .map_err(|e| NetError::from(format!("bad headers received: {e:?}")))?
        .map(usize::from_str)
        .transpose()
        .map_err(|e| NetError::from(format!("bad headers received: {e:?}")))
}

pub fn build_response(
    response: http::response::Parts,
    content_length: Option<usize>,
    response_bytes: Vec<u8>,
) -> Result<DnsResponse, NetError> {
    if let Some(content_length) = content_length
        && response_bytes.len() != content_length
    {
        return Err(NetError::from(format!(
            "expected byte length: {}, got: {}",
            content_length,
            response_bytes.len()
        )));
    }

    if !response.status.is_success() {
        let error_string = String::from_utf8_lossy(response_bytes.as_ref());

        return Err(NetError::from(format!(
            "http unsuccessful code: {}, message: {}",
            response.status, error_string
        )));
    }
    let content_type = response
        .headers
        .get(http::header::CONTENT_TYPE)
        .map(|h| {
            h.to_str()
                .map_err(|err| NetError::from(format!("ContentType header not a string: {err}")))
        })
        .unwrap_or(Ok(MIME_APPLICATION_DNS))?;

    if content_type != MIME_APPLICATION_DNS {
        return Err(NetError::from(format!(
            "ContentType unsupported (must be '{}'): '{}'",
            MIME_APPLICATION_DNS, content_type
        )));
    }

    DnsResponse::from_buffer(response_bytes.to_vec()).map_err(NetError::from)
}
