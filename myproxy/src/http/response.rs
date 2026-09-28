//! Response-side parsing: read the upstream response head and classify the
//! body length. The body length decides when the response is over (needed
//! to reuse the upstream connection); an unknown length means the body
//! must be relayed some other way (chunked framing) or until EOF
//! (connection cannot be pooled).

use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::request::HeadError;
use super::MAX_HEADERS;

pub struct ResponseHead {
    /// Bytes of the head inside the buffer it was parsed from; the rest of
    /// the buffer belongs to the body.
    pub raw_len: usize,
    pub content_length: Option<u64>,
    /// False when the response carries `Connection: close` or HTTP/1.0.
    pub keep_alive: bool,
    /// `Transfer-Encoding: chunked` (Content-Length is unreliable then).
    pub chunked: bool,
    /// 1xx/204/304-style responses that have no body regardless of headers.
    pub no_body_status: bool,
}

pub enum RespError {
    // The inner error is contextual (Io vs TooLarge vs Invalid variants);
    // callers only branch on Closed, the rest is logged/forwarded as-is.
    #[allow(dead_code)]
    Head(HeadError),
    /// Clean EOF with zero bytes read: pooled connection died while idle.
    Closed,
}

/// Reads one response head from `sock` into `buf` (clearing it first).
/// Same growth/read loop as the request-side `read_head`, but parses with
/// `httparse::Response`. A clean EOF before any byte returns
/// [`RespError::Closed`] — the signature of a stale pooled connection.
pub async fn read_response_head(
    sock: &mut TcpStream,
    buf: &mut Vec<u8>,
    max_bytes: usize,
    idle: Duration,
) -> Result<ResponseHead, RespError> {
    buf.clear();
    loop {
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut resp = httparse::Response::new(&mut headers);
        let parsed = match resp.parse(buf) {
            Ok(httparse::Status::Complete(len)) => Some(len),
            Ok(httparse::Status::Partial) => None,
            Err(_) => return Err(RespError::Head(HeadError::Invalid)),
        };
        if let Some(len) = parsed {
            let mut content_length = None;
            let mut close = false;
            let mut chunked = false;
            for h in resp.headers.iter() {
                if h.name.eq_ignore_ascii_case("content-length") {
                    if let Some(v) = std::str::from_utf8(h.value)
                        .ok()
                        .and_then(|s| s.trim().parse::<u64>().ok())
                    {
                        content_length = Some(v);
                    }
                } else if h.name.eq_ignore_ascii_case("connection") {
                    if h.value
                        .to_ascii_lowercase()
                        .windows(5)
                        .any(|w| w == b"close")
                    {
                        close = true;
                    }
                } else if h.name.eq_ignore_ascii_case("transfer-encoding") {
                    if h.value
                        .to_ascii_lowercase()
                        .windows(7)
                        .any(|w| w == b"chunked")
                    {
                        chunked = true;
                    }
                }
            }
            let status = resp.code.unwrap_or(0);
            let version11 = resp.version == Some(1);
            let no_body_status: bool =
                status == 204 || status == 304 || (100..200).contains(&status);
            return Ok(ResponseHead {
                raw_len: len,
                content_length: if chunked { None } else { content_length },
                keep_alive: version11 && !close,
                chunked,
                no_body_status,
            });
        }
        if buf.len() >= max_bytes {
            return Err(RespError::Head(HeadError::TooLarge));
        }
        buf.reserve(2048);
        let n = match timeout(idle, sock.read_buf(buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(RespError::Head(HeadError::Io(e))),
            Err(_) => return Err(RespError::Head(HeadError::Io(err_timedout()))),
        };
        if n == 0 {
            // Clean EOF with zero bytes: stale pooled connection.
            return Err(RespError::Closed);
        }
    }
}

fn err_timedout() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::TimedOut)
}
