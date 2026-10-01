//! io_uring prototype of myproxy's HTTP/1 keep-alive hot path (monoio).
//!
//! Purpose: measure, on the same benchmark matrix, what io_uring buys on
//! the requests path before considering a migration — not a product.
//! Scope = what http-1kb / http-10kb / http-1kb-short exercise:
//! absolute-form requests, Basic auth (key `secret1`), header rewrite,
//! one kept-alive upstream connection per client connection, responses
//! framed by Content-Length. Anything else gets 400/501 and a close.
//!
//! Same choices as myproxy where they matter for a fair comparison:
//! TCP_NODELAY, reused per-connection buffers (no per-request allocation),
//! one read of up to 16 KB for the response head + small body, and an
//! idle timeout armed on every read/write (myproxy pays a timer per I/O).

use std::net::SocketAddr;

use base64::Engine as _;
use monoio::buf::{IoBuf, IoBufMut};
use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::{TcpListener, TcpStream};

const KEY: &[u8] = b"secret1";
const RESP_400: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_407: &[u8] = b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"p\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_502: &[u8] =
    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

fn main() {
    let auth = std::env::var("AUTH").map(|v| v != "off").unwrap_or(true);
    let listen = std::env::var("LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let mut rt = monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
        .with_entries(4096)
        .enable_timer()
        .build()
        .expect("io_uring runtime (seccomp must allow io_uring_*)");
    rt.block_on(async move {
        let l = TcpListener::bind(listen.as_str()).expect("bind");
        eprintln!("uring-proto listening on {listen} auth={auth}");
        loop {
            let Ok((s, _)) = l.accept().await else {
                continue;
            };
            let _ = s.set_nodelay(true);
            monoio::spawn(async move {
                let _ = conn(s, auth).await;
            });
        }
    });
}

/// Reads more bytes at the end of `buf` (owned-buffer API: the vector is
/// handed to the ring and given back).
const IDLE: std::time::Duration = std::time::Duration::from_secs(60);

/// Idle timeout around an owned-buffer operation. On expiry the buffer is
/// gone with the cancelled op: the connection is dropped anyway.
async fn timed<F, T>(fut: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    monoio::time::timeout(IDLE, fut).await.ok()
}

async fn read_more(
    s: &mut TcpStream,
    mut buf: Vec<u8>,
    grow: usize,
) -> (std::io::Result<usize>, Vec<u8>) {
    if buf.capacity() - buf.len() < grow / 2 {
        buf.reserve(grow);
    }
    let (len, cap) = (buf.len(), buf.capacity());
    match timed(s.read(buf.slice_mut(len..cap))).await {
        Some((r, slice)) => (r, slice.into_inner()),
        None => (
            Err(std::io::ErrorKind::TimedOut.into()),
            Vec::with_capacity(grow),
        ),
    }
}

async fn write(s: &mut TcpStream, buf: Vec<u8>, range: std::ops::Range<usize>) -> (bool, Vec<u8>) {
    let cap = buf.capacity();
    match timed(s.write_all(buf.slice(range))).await {
        Some((r, slice)) => (r.is_ok(), slice.into_inner()),
        None => (false, Vec::with_capacity(cap)),
    }
}

fn basic_ok(value: &[u8]) -> bool {
    if value.len() < 6 || !value[..6].eq_ignore_ascii_case(b"Basic ") {
        return false;
    }
    let mut out = [0u8; 192];
    match base64::engine::general_purpose::STANDARD.decode_slice(value[6..].trim_ascii(), &mut out)
    {
        Ok(n) => {
            let d = &out[..n];
            let key = d
                .iter()
                .position(|&b| b == b':')
                .map(|i| &d[i + 1..])
                .unwrap_or(d);
            key == KEY
        }
        Err(_) => false,
    }
}

/// One client connection: keep-alive loop.
async fn conn(mut c: TcpStream, auth: bool) -> Option<()> {
    let mut req: Vec<u8> = Vec::with_capacity(4096);
    let mut out: Vec<u8> = Vec::with_capacity(1024);
    let mut resp: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut up: Option<(SocketAddr, TcpStream)> = None;
    loop {
        // ---- request head
        req.clear();
        let (head_len, target, close) = loop {
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut r = httparse::Request::new(&mut headers);
            match r.parse(&req) {
                Ok(httparse::Status::Complete(n)) => {
                    if auth
                        && !r.headers.iter().any(|h| {
                            h.name.eq_ignore_ascii_case("proxy-authorization") && basic_ok(h.value)
                        })
                    {
                        let (_, _) = write(&mut c, RESP_407.to_vec(), 0..RESP_407.len()).await;
                        return None;
                    }
                    let path = r.path.unwrap_or("");
                    let Some(rest) = path.strip_prefix("http://") else {
                        let (_, _) = write(&mut c, RESP_400.to_vec(), 0..RESP_400.len()).await;
                        return None;
                    };
                    let (authority, origin) = match rest.find('/') {
                        Some(i) => (&rest[..i], &rest[i..]),
                        None => (rest, "/"),
                    };
                    let addr: Option<SocketAddr> = authority
                        .parse()
                        .ok()
                        .or_else(|| format!("{authority}:80").parse().ok());
                    let Some(addr) = addr else {
                        let (_, _) = write(&mut c, RESP_400.to_vec(), 0..RESP_400.len()).await;
                        return None;
                    };
                    let close = r.headers.iter().any(|h| {
                        h.name.eq_ignore_ascii_case("connection")
                            && h.value.eq_ignore_ascii_case(b"close")
                    });
                    // Rewritten head (origin-form, proxy headers dropped).
                    out.clear();
                    out.extend_from_slice(r.method.unwrap_or("GET").as_bytes());
                    out.push(b' ');
                    out.extend_from_slice(origin.as_bytes());
                    out.extend_from_slice(b" HTTP/1.1\r\n");
                    for h in r.headers.iter() {
                        let n = h.name;
                        if n.eq_ignore_ascii_case("proxy-authorization")
                            || n.eq_ignore_ascii_case("proxy-connection")
                            || n.eq_ignore_ascii_case("connection")
                            || n.eq_ignore_ascii_case("keep-alive")
                        {
                            continue;
                        }
                        out.extend_from_slice(n.as_bytes());
                        out.extend_from_slice(b": ");
                        out.extend_from_slice(h.value);
                        out.extend_from_slice(b"\r\n");
                    }
                    out.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
                    break (n, addr, close);
                }
                Ok(httparse::Status::Partial) => {}
                Err(_) => {
                    let (_, _) = write(&mut c, RESP_400.to_vec(), 0..RESP_400.len()).await;
                    return None;
                }
            }
            let (r, b) = read_more(&mut c, req, 4096).await;
            req = b;
            match r {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
        };
        if req.len() > head_len {
            return None; // request bodies / pipelining: out of the prototype's scope
        }

        // ---- upstream (kept alive while the client targets the same origin)
        if up.as_ref().map(|(a, _)| *a) != Some(target) {
            match TcpStream::connect(target).await {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    up = Some((target, s));
                }
                Err(_) => {
                    let (_, _) = write(&mut c, RESP_502.to_vec(), 0..RESP_502.len()).await;
                    return None;
                }
            }
        }
        let (_, u) = up.as_mut()?;
        let n = out.len();
        let (ok, b) = write(u, out, 0..n).await;
        out = b;
        if !ok {
            let (_, _) = write(&mut c, RESP_502.to_vec(), 0..RESP_502.len()).await;
            return None;
        }

        // ---- response head (+ the body bytes that came with it)
        resp.clear();
        let (head_end, body_len, keep) = loop {
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut r = httparse::Response::new(&mut headers);
            if let Ok(httparse::Status::Complete(n)) = r.parse(&resp) {
                let cl = r
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                    .and_then(|h| {
                        std::str::from_utf8(h.value)
                            .ok()?
                            .trim()
                            .parse::<usize>()
                            .ok()
                    });
                let Some(cl) = cl else {
                    return None; // chunked / until-EOF: out of scope
                };
                let keep = r.version == Some(1)
                    && !r.headers.iter().any(|h| {
                        h.name.eq_ignore_ascii_case("connection")
                            && h.value.eq_ignore_ascii_case(b"close")
                    });
                break (n, cl, keep);
            }
            let (r, b) = read_more(u, resp, 16 * 1024).await;
            resp = b;
            match r {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
        };
        let total = head_end + body_len;
        let have = resp.len().min(total);
        let (ok, b) = write(&mut c, resp, 0..have).await;
        resp = b;
        if !ok {
            return None;
        }
        // ---- rest of the body
        let mut left = total - have;
        while left > 0 {
            resp.clear();
            let cap = resp.capacity().min(left);
            let (r, slice) = timed(u.read(resp.slice_mut(0..cap))).await?;
            resp = slice.into_inner();
            let n = match r {
                Ok(0) | Err(_) => return None,
                Ok(n) => n,
            };
            let (ok, b) = write(&mut c, resp, 0..n).await;
            resp = b;
            if !ok {
                return None;
            }
            left -= n;
        }
        if !keep {
            up = None;
        }
        if close {
            return None;
        }
    }
}
