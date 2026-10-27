//! Request-side parsing: read the head, classify it into the two proxy
//! targets (CONNECT tunnel / origin-form HTTP), rewrite it for the upstream.

use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::time::timeout;

use super::MAX_HEADERS;

#[derive(Debug)]
pub enum HeadError {
    Io(std::io::Error),
    TooLarge,
    Invalid,
}

/// A byte range inside the buffer a head was parsed from. Plain indices —
/// `RequestHead` owns no allocation and no borrow; consumers slice through
/// the buffer (`span.slice(buf)`). Zero-alloc parsing: method, path, auth
/// and host are spans, not heap Strings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    #[inline]
    pub fn slice<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        &buf[self.start..self.end]
    }

    #[inline]
    #[cfg_attr(not(test), allow(dead_code))] // used by the classify tests
    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

/// A parsed HTTP request head. `raw_len` is the number of bytes of the head
/// in the buffer it was parsed from; everything after is treated as body
/// bytes. Every text field is a [`Span`] into that same buffer.
pub struct RequestHead {
    pub raw_len: usize,
    pub method: Span,
    pub path: Span,
    pub version_minor: u8,
    pub auth: Option<Span>,
    pub host: Option<Span>,
    pub content_length: Option<u64>,
    /// `Transfer-Encoding: chunked` body (relayed verbatim upstream).
    pub chunked: bool,
    pub close: bool,
    /// `Connection: Upgrade` with `Upgrade: websocket` (a WebSocket handshake).
    pub upgrade: bool,
}

/// The proxy target derived from a head: CONNECT `host:port` -> raw tunnel,
/// or origin-form HTTP request to the upstream authority. Host/origin are
/// spans into the buffer the head was parsed from.
pub enum Target {
    /// CONNECT host:port -> TCP tunnel.
    Connect { host: Span, port: u16 },
    /// Plain HTTP request to host:port, rewritten request line kept in `origin`
    /// (empty span = no path in the absolute form; rewrite emits "/").
    Http { host: Span, port: u16, origin: Span },
}

/// Reads one request head from `sock` into `buf` (clearing it first).
/// Returns `None` on clean EOF before any byte of a request.
pub async fn read_head<S: tokio::io::AsyncRead + Unpin>(
    sock: &mut S,
    buf: &mut Vec<u8>,
    max_bytes: usize,
    idle: Duration,
    head_timeout: Duration,
) -> Result<Option<RequestHead>, HeadError> {
    buf.clear();
    // Slowloris: once part of a head is buffered, the rest must arrive
    // within `head_timeout` in total (not per read). Armed only when a head
    // spans several reads — the usual one-read head never computes it.
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        // Zero-borrow parse: spans are computed inside this block from the
        // very buffer; the returned head owns plain indices only.
        let parsed: Option<RequestHead> = {
            let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
            let mut req = httparse::Request::new(&mut headers);
            match req.parse(buf) {
                Ok(httparse::Status::Complete(len)) => {
                    Some(RequestHead::from(&req, len, buf).ok_or(HeadError::Invalid)?)
                }
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
        if !buf.is_empty() && deadline.is_none() {
            deadline = Some(tokio::time::Instant::now() + head_timeout);
        }
        buf.reserve(2048);
        let read = sock.read_buf(buf);
        let n = match deadline {
            None => timeout(idle, read).await,
            Some(d) => tokio::time::timeout_at(d, read).await,
        }
        .map_err(|_| HeadError::Io(std::io::Error::from(std::io::ErrorKind::TimedOut)))?
        .map_err(HeadError::Io)?;
        if n == 0 {
            return Ok(None);
        }
    }
}

impl RequestHead {
    /// `None` = ambiguous body framing (request smuggling vectors):
    /// Transfer-Encoding other than a single plain `chunked`, TE together
    /// with Content-Length, TE on HTTP/1.0, an invalid Content-Length, or
    /// conflicting duplicates.
    fn from(req: &httparse::Request, len: usize, buf: &[u8]) -> Option<Self> {
        let mut auth = None;
        let mut host = None;
        let mut content_length = None;
        let mut chunked = false;
        let mut close = false;
        let mut conn_upgrade = false;
        let mut upgrade_ws = false;
        for h in req.headers.iter() {
            if h.name.eq_ignore_ascii_case("proxy-authorization") {
                if auth.is_none() {
                    auth = Some(span_of(buf, h.value));
                }
            } else if h.name.eq_ignore_ascii_case("host") {
                if host.is_none() {
                    host = Some(span_of(buf, h.value));
                }
            } else if h.name.eq_ignore_ascii_case("content-length") {
                let v = parse_content_length(h.value)?;
                if content_length.is_some_and(|cl| cl != v) {
                    return None;
                }
                content_length = Some(v);
            } else if h.name.eq_ignore_ascii_case("transfer-encoding") {
                if chunked || !h.value.trim_ascii().eq_ignore_ascii_case(b"chunked") {
                    return None;
                }
                chunked = true;
            } else if h.name.eq_ignore_ascii_case("connection") {
                close |= contains_token(h.value, "close");
                conn_upgrade |= contains_token(h.value, "upgrade");
            } else if h.name.eq_ignore_ascii_case("upgrade") {
                upgrade_ws |= contains_token(h.value, "websocket");
            }
        }
        if chunked && (content_length.is_some() || req.version == Some(0)) {
            return None;
        }
        Some(RequestHead {
            raw_len: len,
            method: span_of(buf, req.method.unwrap_or("").as_bytes()),
            path: span_of(buf, req.path.unwrap_or("").as_bytes()),
            version_minor: req.version.unwrap_or(1),
            auth,
            host,
            content_length,
            chunked,
            close,
            upgrade: conn_upgrade && upgrade_ws,
        })
    }
}

/// Strict Content-Length: 1..=19 ASCII digits, optional surrounding
/// whitespace (no sign, no list — `u64::from_str` would accept `+5`).
fn parse_content_length(v: &[u8]) -> Option<u64> {
    let v = v.trim_ascii();
    if v.is_empty() || v.len() > 19 || !v.iter().all(u8::is_ascii_digit) {
        return None;
    }
    v.iter().try_fold(0u64, |acc, &d| {
        acc.checked_mul(10)?.checked_add((d - b'0') as u64)
    })
}

/// Byte range of `s` inside `buf` (httparse returns zero-copy slices into
/// the parsed buffer; we recover their offsets).
///
/// SAFETY: `s` always points inside `buf`'s allocation — it is a subslice
/// httparse produced from this very buffer — so `offset_from` stays within
/// the same allocation. Empty strings (httparse's static fallbacks) are
/// short-circuited: two pointers from different allocations would make
/// `offset_from` meaningless.
fn span_of(buf: &[u8], s: &[u8]) -> Span {
    if s.is_empty() {
        return Span { start: 0, end: 0 };
    }
    let start = unsafe { s.as_ptr().offset_from(buf.as_ptr()) } as usize;
    debug_assert!(start + s.len() <= buf.len());
    Span {
        start,
        end: start + s.len(),
    }
}

pub(super) fn contains_token(value: &[u8], token: &str) -> bool {
    std::str::from_utf8(value)
        .map(|s| s.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
        .unwrap_or(false)
}

/// Routes a parsed head: CONNECT -> tunnel target; absolute-form or
/// Host-header form -> origin-form HTTP target.
pub fn classify(head: &RequestHead, buf: &[u8]) -> Result<Target, &'static str> {
    let path = head.path.slice(buf);
    if head.method.slice(buf).eq_ignore_ascii_case(b"connect") {
        let (host, port) = parse_authority(buf, head.path)?;
        Ok(Target::Connect {
            host,
            port: port.unwrap_or(443),
        })
    } else if let Some(rest) = strip_prefix_bytes(path, b"http://") {
        let slash = rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
        let (host, port) = parse_authority(
            buf,
            Span {
                start: head.path.start + 7,
                end: head.path.start + 7 + slash,
            },
        )?;
        let origin = Span {
            start: head.path.start + 7 + slash,
            end: head.path.end,
        };
        Ok(Target::Http {
            host,
            port: port.unwrap_or(80),
            origin,
        })
    } else if path.first() == Some(&b'/') {
        let hs = head.host.ok_or("missing Host header")?;
        let (host, port) = parse_authority(buf, hs)?;
        Ok(Target::Http {
            host,
            port: port.unwrap_or(80),
            origin: head.path,
        })
    } else {
        Err("unsupported request target")
    }
}

/// `strip_prefix` on raw bytes; returns the remaining subslice.
fn strip_prefix_bytes<'a>(s: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if s.starts_with(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Parses `host:port`, `[v6]:port` or bare host/IPv6. Returns `(host, port)`
/// as a sub-span of `a` (offsets are absolute into `buf`).
fn parse_authority(buf: &[u8], a: Span) -> Result<(Span, Option<u16>), &'static str> {
    let s = a.slice(buf);
    if s.first() == Some(&b'[') {
        let close = s
            .iter()
            .position(|&b| b == b']')
            .ok_or("bad ipv6 authority")?;
        // Content inside the brackets: s[1..close] (close = ']' index).
        let host = Span {
            start: a.start + 1,
            end: a.start + close,
        };
        let rest = &s[close + 1..];
        let port = match rest.first() {
            Some(b':') => Some(
                std::str::from_utf8(&rest[1..])
                    .map_err(|_| "bad port")?
                    .parse()
                    .map_err(|_| "bad port")?,
            ),
            _ => None,
        };
        Ok((host, port))
    } else if s.iter().filter(|&&b| b == b':').count() >= 2 {
        // Bare IPv6 without port.
        Ok((a, None))
    } else {
        match s.iter().rposition(|&b| b == b':') {
            Some(i) => {
                let host = Span {
                    start: a.start,
                    end: a.start + i,
                };
                let port = std::str::from_utf8(&s[i + 1..])
                    .map_err(|_| "bad port")?
                    .parse()
                    .map_err(|_| "bad port")?;
                Ok((host, Some(port)))
            }
            None => Ok((a, None)),
        }
    }
}

/// Rewrites a raw HTTP/1.x request head into an origin-form request for the
/// upstream, dropping proxy-specific headers and forcing `Connection:
/// keep-alive` (the pool decides per-response whether the connection ends
/// up reused or dropped). Zero-alloc: writes into a connection-owned
/// scratch buffer cleared then reused across keep-alive requests. `method`
/// and `origin` are raw spans/slices from the head's buffer; an empty
/// origin (absolute form without a path) is emitted as "/". `upgrade`: a
/// WebSocket handshake, forwarded with `Connection: Upgrade` (and its
/// `Upgrade` / `Sec-WebSocket-*` headers) instead of `keep-alive`.
pub fn rewrite_head_into(
    out: &mut Vec<u8>,
    head_raw: &[u8],
    method: &[u8],
    origin: &[u8],
    upgrade: bool,
) {
    let conn: &[u8] = if upgrade {
        b"Connection: Upgrade\r\n\r\n"
    } else {
        b"Connection: keep-alive\r\n\r\n"
    };
    out.clear();
    out.reserve(head_raw.len() + 64);
    out.extend_from_slice(method);
    out.push(b' ');
    if origin.is_empty() {
        out.push(b'/');
    } else {
        out.extend_from_slice(origin);
    }
    out.extend_from_slice(b" HTTP/1.1\r\n");

    let rest = match head_raw.iter().position(|&b| b == b'\n') {
        Some(i) => &head_raw[i + 1..],
        None => {
            out.extend_from_slice(conn);
            return;
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
    out.extend_from_slice(conn);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a head whose spans point into `buf` (offsets must be computed
    /// by hand in the tests — same contract the parser fulfils).
    fn head(
        method: (usize, usize),
        path: (usize, usize),
        host: Option<(usize, usize)>,
    ) -> RequestHead {
        RequestHead {
            raw_len: 0,
            method: Span {
                start: method.0,
                end: method.1,
            },
            path: Span {
                start: path.0,
                end: path.1,
            },
            version_minor: 1,
            auth: None,
            host: host.map(|(s, e)| Span { start: s, end: e }),
            content_length: None,
            chunked: false,
            close: false,
            upgrade: false,
        }
    }

    #[test]
    fn classify_connect() {
        let buf = b"CONNECT example.com:443";
        let h = head((0, 7), (8, 23), None);
        match classify(&h, buf).unwrap() {
            Target::Connect { host, port } => {
                assert_eq!(host.slice(buf), b"example.com");
                assert_eq!(port, 443);
            }
            _ => panic!("expected Connect"),
        }
    }

    #[test]
    fn classify_connect_default_port() {
        let buf = b"CONNECT example.com";
        let h = head((0, 7), (8, 19), None);
        match classify(&h, buf).unwrap() {
            Target::Connect { port, .. } => assert_eq!(port, 443),
            _ => panic!("expected Connect"),
        }
    }

    #[test]
    fn classify_absolute_form() {
        let buf = b"GET http://upstream:8000/1kb?x=1";
        let h = head((0, 3), (4, 32), None);
        match classify(&h, buf).unwrap() {
            Target::Http { host, port, origin } => {
                assert_eq!(host.slice(buf), b"upstream");
                assert_eq!(port, 8000);
                assert_eq!(origin.slice(buf), b"/1kb?x=1");
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn classify_absolute_form_no_path() {
        let buf = b"GET http://upstream:8000";
        let h = head((0, 3), (4, 24), None);
        match classify(&h, buf).unwrap() {
            Target::Http { origin, .. } => assert!(origin.is_empty()),
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn classify_host_header_form() {
        // path span (4..8) + host value span (25..38) inside the raw head.
        let buf = b"GET /1kb HTTP/1.1\r\nHost: upstream:8000";
        let h = head((0, 3), (4, 8), Some((25, 38)));
        match classify(&h, buf).unwrap() {
            Target::Http { host, port, origin } => {
                assert_eq!(host.slice(buf), b"upstream");
                assert_eq!(port, 8000);
                assert_eq!(origin.slice(buf), b"/1kb");
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn classify_ipv6_authority() {
        // Bare IPv6 without port.
        let buf = b"GET http://2001:db8::1/";
        let h = head((0, 3), (4, 22), None);
        match classify(&h, buf).unwrap() {
            Target::Http { host, port, .. } => {
                assert_eq!(host.slice(buf), b"2001:db8::1");
                assert_eq!(port, 80);
            }
            _ => panic!("expected Http"),
        }
        // Bracketed IPv6 with port (CONNECT form).
        let buf = b"CONNECT [2001:db8::1]:8443";
        let h = head((0, 7), (8, 26), None);
        match classify(&h, buf).unwrap() {
            Target::Connect { host, port } => {
                assert_eq!(host.slice(buf), b"2001:db8::1");
                assert_eq!(port, 8443);
            }
            _ => panic!("expected Connect"),
        }
    }

    #[test]
    fn parse_read_head_spans() {
        // End-to-end: parse a real head, then slice through the buffer.
        let raw = b"GET http://upstream:8000/1kb HTTP/1.1\r\nProxy-Authorization: Basic abc\r\nHost: upstream:8000\r\nConnection: close\r\n\r\nbody";
        let buf = raw.to_vec();
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut headers);
        let len = match req.parse(&buf).unwrap() {
            httparse::Status::Complete(len) => len,
            _ => panic!("expected complete"),
        };
        let head = RequestHead::from(&req, len, &buf).unwrap();
        assert_eq!(head.method.slice(&buf), b"GET");
        assert_eq!(head.path.slice(&buf), b"http://upstream:8000/1kb");
        assert_eq!(head.raw_len, raw.len() - 4); // body = "body"
        assert!(head.close);
        let auth = head.auth.unwrap();
        assert_eq!(auth.slice(&buf), b"Basic abc");
        let host = head.host.unwrap();
        assert_eq!(host.slice(&buf), b"upstream:8000");
        // Body bytes survive after the head.
        assert_eq!(&buf[head.raw_len..], b"body");
    }

    #[test]
    fn rewrite_drops_proxy_headers() {
        let raw = b"GET http://upstream:8000/1kb HTTP/1.1\r\nHost: upstream:8000\r\nProxy-Authorization: Basic abc\r\nProxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n";
        let mut out = Vec::new();
        rewrite_head_into(&mut out, raw, b"GET", b"/1kb", false);
        let out = String::from_utf8_lossy(&out);
        assert!(out.starts_with("GET /1kb HTTP/1.1\r\n"));
        assert!(out.contains("Host: upstream:8000"));
        assert!(out.contains("Accept: */*"));
        assert!(!out.contains("Proxy-Authorization"));
        assert!(!out.contains("Proxy-Connection"));
        assert!(out.ends_with("Connection: keep-alive\r\n\r\n"));
    }

    #[test]
    fn rewrite_empty_origin_is_root() {
        let raw = b"GET http://upstream:8000 HTTP/1.1\r\n\r\n";
        let mut out = Vec::new();
        rewrite_head_into(&mut out, raw, b"GET", b"", false);
        let out = String::from_utf8_lossy(&out);
        assert!(out.starts_with("GET / HTTP/1.1\r\n"));
    }

    fn parse(raw: &[u8]) -> Option<RequestHead> {
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut headers);
        let len = match req.parse(raw).unwrap() {
            httparse::Status::Complete(n) => n,
            httparse::Status::Partial => panic!("partial"),
        };
        RequestHead::from(&req, len, raw)
    }

    #[test]
    fn ambiguous_body_framing_rejected() {
        let ok = parse(b"POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n");
        assert_eq!(ok.unwrap().content_length, Some(5));
        let c = parse(b"POST / HTTP/1.1\r\nTransfer-Encoding:  Chunked \r\n\r\n").unwrap();
        assert!(c.chunked && c.content_length.is_none());
        for bad in [
            &b"POST / HTTP/1.1\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n",
            b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"POST / HTTP/1.1\r\nTransfer-Encoding: identity\r\n\r\n",
            b"POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: +5\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: abc\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: 5, 5\r\n\r\n",
        ] {
            assert!(parse(bad).is_none(), "{}", String::from_utf8_lossy(bad));
        }
    }

    fn head_of(raw: &[u8]) -> RequestHead {
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut headers);
        let len = match req.parse(raw).unwrap() {
            httparse::Status::Complete(l) => l,
            httparse::Status::Partial => panic!("partial head"),
        };
        RequestHead::from(&req, len, raw).unwrap()
    }

    #[test]
    fn websocket_handshake_is_detected() {
        let ws = b"GET http://h/ws HTTP/1.1\r\nHost: h\r\nConnection: keep-alive, Upgrade\r\nUpgrade: WebSocket\r\n\r\n";
        assert!(head_of(ws).upgrade);
        // Each header alone is not a handshake, nor is another protocol.
        for raw in [
            &b"GET http://h/ HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\n\r\n"[..],
            b"GET http://h/ HTTP/1.1\r\nHost: h\r\nConnection: Upgrade\r\n\r\n",
            b"GET http://h/ HTTP/1.1\r\nHost: h\r\nConnection: Upgrade\r\nUpgrade: h2c\r\n\r\n",
        ] {
            assert!(!head_of(raw).upgrade, "{}", String::from_utf8_lossy(raw));
        }
        // `close` survives a second Connection header.
        let two = b"GET http://h/ HTTP/1.1\r\nHost: h\r\nConnection: close\r\nConnection: Upgrade\r\n\r\n";
        assert!(head_of(two).close);
    }

    #[test]
    fn rewrite_keeps_websocket_headers_and_upgrades_the_connection() {
        let raw = b"GET http://h/ws HTTP/1.1\r\nHost: h\r\nProxy-Authorization: Basic x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: k\r\n\r\n";
        let mut out = Vec::new();
        rewrite_head_into(&mut out, raw, b"GET", b"/ws", true);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "GET /ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nSec-WebSocket-Key: k\r\nConnection: Upgrade\r\n\r\n"
        );
    }
}
