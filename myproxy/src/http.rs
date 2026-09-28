use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

const MAX_HEADERS: usize = 64;

pub enum HeadError {
    Io(std::io::Error),
    TooLarge,
    Invalid,
}

/// A parsed HTTP request head. `raw_len` is the number of bytes of the head in
/// the buffer it was parsed from; everything after is treated as body bytes.
pub struct RequestHead {
    pub raw_len: usize,
    pub method: String,
    pub path: String,
    pub version_minor: u8,
    pub auth: Option<Vec<u8>>,
    pub host: Option<String>,
    pub content_length: Option<u64>,
    pub close: bool,
}

pub enum Target {
    /// CONNECT host:port -> TCP tunnel.
    Connect { host: String, port: u16 },
    /// Plain HTTP request to host:port, rewritten request line kept in `origin`.
    Http { host: String, port: u16, origin: String },
}

/// Reads one request head from `sock` into `buf` (clearing it first).
/// Returns `None` on clean EOF before any byte of a request.
pub async fn read_head(
    sock: &mut TcpStream,
    buf: &mut Vec<u8>,
    max_bytes: usize,
    idle: Duration,
) -> Result<Option<RequestHead>, HeadError> {
    buf.clear();
    loop {
        let parsed = {
            let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
            let mut req = httparse::Request::new(&mut headers);
            match req.parse(buf) {
                Ok(httparse::Status::Complete(len)) => Some(RequestHead::from(&req, len)),
                Ok(httparse::Status::Partial) => None,
                Err(_) => return Err(HeadError::Invalid),
            }
        };
        if let Some(head) = parsed {
            return Ok(Some(head));
        }
        if buf.len() >= max_bytes {
            return Err(HeadError::TooLarge);
        }
        buf.reserve(2048);
        let n = timeout(idle, sock.read_buf(buf))
            .await
            .map_err(|_| HeadError::Io(std::io::Error::from(std::io::ErrorKind::TimedOut)))?
            .map_err(HeadError::Io)?;
        if n == 0 {
            return Ok(None);
        }
    }
}

impl RequestHead {
    fn from(req: &httparse::Request, len: usize) -> Self {
        let mut auth = None;
        let mut host = None;
        let mut content_length = None;
        let mut close = false;
        for h in req.headers.iter() {
            if h.name.eq_ignore_ascii_case("proxy-authorization") {
                if auth.is_none() {
                    auth = Some(h.value.to_vec());
                }
            } else if h.name.eq_ignore_ascii_case("host") {
                if host.is_none() {
                    host = Some(String::from_utf8_lossy(h.value).into_owned());
                }
            } else if h.name.eq_ignore_ascii_case("content-length") {
                if content_length.is_none() {
                    content_length = std::str::from_utf8(h.value)
                        .ok()
                        .and_then(|s| s.trim().parse().ok());
                }
            } else if h.name.eq_ignore_ascii_case("connection") {
                close = contains_token(h.value, "close");
            }
        }
        RequestHead {
            raw_len: len,
            method: req.method.unwrap_or("").to_string(),
            path: req.path.unwrap_or("").to_string(),
            version_minor: req.version.unwrap_or(1),
            auth,
            host,
            content_length,
            close,
        }
    }
}

fn contains_token(value: &[u8], token: &str) -> bool {
    std::str::from_utf8(value)
        .map(|s| s.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
        .unwrap_or(false)
}

pub fn classify(head: &RequestHead) -> Result<Target, &'static str> {
    if head.method.eq_ignore_ascii_case("connect") {
        let (host, port) = parse_authority(&head.path)?;
        Ok(Target::Connect {
            host,
            port: port.unwrap_or(443),
        })
    } else if let Some(rest) = head.path.strip_prefix("http://") {
        let slash = rest.find('/').unwrap_or(rest.len());
        let (host, port) = parse_authority(&rest[..slash])?;
        let origin = if slash == rest.len() {
            "/".to_string()
        } else {
            rest[slash..].to_string()
        };
        Ok(Target::Http {
            host,
            port: port.unwrap_or(80),
            origin,
        })
    } else if head.path.starts_with('/') {
        let (host, port) = match &head.host {
            Some(h) => parse_authority(h)?,
            None => return Err("missing Host header"),
        };
        Ok(Target::Http {
            host,
            port: port.unwrap_or(80),
            origin: head.path.clone(),
        })
    } else {
        Err("unsupported request target")
    }
}

/// Parses `host:port`, `[v6]:port` or bare host/IPv6. Returns `(host, port)`.
fn parse_authority(a: &str) -> Result<(String, Option<u16>), &'static str> {
    if let Some(rest) = a.strip_prefix('[') {
        let close = rest.find(']').ok_or("bad ipv6 authority")?;
        let host = rest[..close].to_string();
        let port = match rest[close + 1..].strip_prefix(':') {
            Some(p) => Some(p.parse().map_err(|_| "bad port")?),
            None => None,
        };
        Ok((host, port))
    } else if a.matches(':').count() >= 2 {
        // Bare IPv6 without port.
        Ok((a.to_string(), None))
    } else {
        match a.rfind(':') {
            Some(i) => Ok((a[..i].to_string(), Some(a[i + 1..].parse().map_err(|_| "bad port")?))),
            None => Ok((a.to_string(), None)),
        }
    }
}

/// Rewrites a raw HTTP/1.x request head into an origin-form request for the
/// upstream, dropping proxy-specific headers and forcing `Connection: close`.
pub fn rewrite_head(head_raw: &[u8], method: &str, origin: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(head_raw.len() + 64);
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(origin.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\n");

    let rest = match head_raw.iter().position(|&b| b == b'\n') {
        Some(i) => &head_raw[i + 1..],
        None => {
            out.extend_from_slice(b"Connection: close\r\n\r\n");
            return out;
        }
    };
    for line in rest.split_inclusive(|&b| b == b'\n') {
        if line == b"\r\n" {
            break;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            break;
        };
        let name = &line[..colon];
        if name.eq_ignore_ascii_case(b"proxy-authorization")
            || name.eq_ignore_ascii_case(b"proxy-connection")
            || name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"keep-alive")
        {
            continue;
        }
        out.extend_from_slice(line);
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_connect() {
        let head = RequestHead {
            raw_len: 0,
            method: "CONNECT".into(),
            path: "example.com:443".into(),
            version_minor: 1,
            auth: None,
            host: Some("example.com:443".into()),
            content_length: None,
            close: false,
        };
        match classify(&head).unwrap() {
            Target::Connect { host, port } => {
                assert_eq!(host, "example.com");
                assert_eq!(port, 443);
            }
            _ => panic!("expected Connect"),
        }
    }

    #[test]
    fn classify_connect_default_port() {
        let head = RequestHead {
            raw_len: 0,
            method: "CONNECT".into(),
            path: "example.com".into(),
            version_minor: 1,
            auth: None,
            host: Some("example.com".into()),
            content_length: None,
            close: false,
        };
        match classify(&head).unwrap() {
            Target::Connect { port, .. } => assert_eq!(port, 443),
            _ => panic!("expected Connect"),
        }
    }

    #[test]
    fn classify_absolute_form() {
        let head = RequestHead {
            raw_len: 0,
            method: "GET".into(),
            path: "http://upstream:8000/1kb?x=1".into(),
            version_minor: 1,
            auth: None,
            host: Some("upstream:8000".into()),
            content_length: None,
            close: false,
        };
        match classify(&head).unwrap() {
            Target::Http { host, port, origin } => {
                assert_eq!(host, "upstream");
                assert_eq!(port, 8000);
                assert_eq!(origin, "/1kb?x=1");
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn rewrite_drops_proxy_headers() {
        let raw = b"GET http://upstream:8000/1kb HTTP/1.1\r\nHost: upstream:8000\r\nProxy-Authorization: Basic abc\r\nProxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n";
        let out = rewrite_head(raw, "GET", "/1kb");
        let out = String::from_utf8_lossy(&out);
        assert!(out.starts_with("GET /1kb HTTP/1.1\r\n"));
        assert!(out.contains("Host: upstream:8000"));
        assert!(out.contains("Accept: */*"));
        assert!(!out.contains("Proxy-Authorization"));
        assert!(!out.contains("Proxy-Connection"));
        assert!(out.ends_with("Connection: close\r\n\r\n"));
    }
}
