//! Programmable HTTP/1.1 origin: the path selects the behaviour.
//!
//! | Path | Response |
//! |---|---|
//! | `/data/<n>?framing=cl\|chunked\|eof&chunk=<k>&trailers=1&seed=<s>` | payload `seed` (default `n`) of `n` bytes |
//! | `/status/<code>` | empty body (no Content-Length for 1xx/204/304) |
//! | `/1xx` | `103 Early Hints` then `200` body `ok` |
//! | `/reset/<n>` | Content-Length `n`, half the body, then RST |
//! | `/hang` | never answers |
//! | `/close-idle` | `ok` keep-alive, then closes the socket once idle 100 ms |
//! | `/close` | `ok` with `Connection: close` |
//! | `/echo` | body = request head as received + `\r\n\r\n` + decoded body; `X-Req-Framing: cl\|chunked\|none` |
//! | `/headers/<n>` | `ok` with an `X-Big` header of `n` bytes |
//! | `/slow/<n>?ms=<t>` | `n` bytes in 10 pieces, `t` ms apart |
//! | `/drop` | reads the request, closes without answering |
//! | `/hits?of=<k>` | body = number of requests received with `key=<k>` |
//! | `/ws?mode=echo\|head\|early\|refuse` | WebSocket handshake: `101` then echo (`head`: also the request head as received; `early`: also a frame sent with the 101); no upgrade headers or `refuse`: `426` + `nope` |
//!
//! Any request with `key=<k>` in its query is counted (retries visible).
//!
//! Every response carries `X-Conn: <connection id>` and `X-Seq: <request
//! index on this connection>` (upstream pooling is visible to clients).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::payload;

pub struct Origin {
    pub v4: SocketAddr,
    pub v6: Option<SocketAddr>,
}

static CONN_ID: AtomicU64 = AtomicU64::new(1);
static HITS: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

fn hit(key: &str) {
    *HITS
        .lock()
        .unwrap()
        .get_or_insert_with(Default::default)
        .entry(key.to_string())
        .or_default() += 1;
}

pub fn hits(key: &str) -> u64 {
    HITS.lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(key).copied())
        .unwrap_or(0)
}

impl Origin {
    pub async fn start() -> anyhow::Result<Self> {
        let (l4, l6) = super::tcp_pair().await?;
        let v4 = l4.local_addr()?;
        let v6 = super::addr_of(&l6);
        tokio::spawn(accept_loop(l4));
        if let Some(l6) = l6 {
            tokio::spawn(accept_loop(l6));
        }
        Ok(Self { v4, v6 })
    }

    /// `host:port` authority on 127.0.0.1.
    pub fn auth(&self) -> String {
        self.v4.to_string()
    }

    /// `http://127.0.0.1:port<path>` (absolute-form target).
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.v4)
    }
}

async fn accept_loop(l: TcpListener) {
    loop {
        let Ok((s, _)) = l.accept().await else {
            continue;
        };
        let _ = s.set_nodelay(true);
        tokio::spawn(serve(s));
    }
}

pub struct Request {
    pub method: String,
    pub target: String,
    pub head: Vec<u8>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub framing: &'static str,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Buffered reader over a stream for HTTP/1.1 framing.
pub struct Wire<S> {
    pub s: S,
    pub buf: Vec<u8>,
}

impl<S: AsyncRead + Unpin> Wire<S> {
    pub fn new(s: S) -> Self {
        Self { s, buf: Vec::new() }
    }

    /// Reads more bytes; Ok(false) on EOF.
    pub async fn fill(&mut self) -> std::io::Result<bool> {
        let mut tmp = vec![0u8; 64 * 1024];
        let n = self.s.read(&mut tmp).await?;
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(n > 0)
    }

    /// Head up to and excluding the blank line; None on clean EOF.
    pub async fn head(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let h = self.buf[..end].to_vec();
                self.buf.drain(..end + 4);
                return Ok(Some(h));
            }
            if !self.fill().await? {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
        }
    }

    pub async fn line(&mut self) -> std::io::Result<Vec<u8>> {
        loop {
            if let Some(end) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let l = self.buf[..end].to_vec();
                self.buf.drain(..end + 2);
                return Ok(l);
            }
            if !self.fill().await? {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
        }
    }

    pub async fn exact(&mut self, n: usize) -> std::io::Result<Vec<u8>> {
        while self.buf.len() < n {
            if !self.fill().await? {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(self.buf.drain(..n).collect())
    }

    pub async fn read_to_close(&mut self) -> std::io::Result<Vec<u8>> {
        while self.fill().await? {}
        Ok(std::mem::take(&mut self.buf))
    }

    /// Decodes a chunked body; returns (body, trailer lines).
    pub async fn chunked(&mut self) -> std::io::Result<(Vec<u8>, Vec<String>)> {
        let mut body = Vec::new();
        loop {
            let l = self.line().await?;
            let l = String::from_utf8_lossy(&l);
            let size = l.split(';').next().unwrap_or("").trim();
            let n = usize::from_str_radix(size, 16)
                .map_err(|_| std::io::Error::other(format!("bad chunk size {l:?}")))?;
            if n == 0 {
                let mut trailers = Vec::new();
                loop {
                    let t = self.line().await?;
                    if t.is_empty() {
                        return Ok((body, trailers));
                    }
                    trailers.push(String::from_utf8_lossy(&t).into_owned());
                }
            }
            body.extend(self.exact(n).await?);
            if !self.exact(2).await?.eq(b"\r\n") {
                return Err(std::io::Error::other("missing CRLF after chunk"));
            }
        }
    }
}

pub fn parse_headers(head: &[u8]) -> (String, Vec<(String, String)>) {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("").to_string();
    let headers = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    (first, headers)
}

async fn read_request<S: AsyncRead + Unpin>(w: &mut Wire<S>) -> std::io::Result<Option<Request>> {
    let Some(head) = w.head().await? else {
        return Ok(None);
    };
    let (first, headers) = parse_headers(&head);
    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut req = Request {
        method,
        target,
        head,
        headers,
        body: Vec::new(),
        framing: "none",
    };
    if req
        .header("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        req.body = w.chunked().await?.0;
        req.framing = "chunked";
    } else if let Some(n) = req.header("content-length").and_then(|v| v.parse().ok()) {
        req.body = w.exact(n).await?;
        req.framing = "cl";
    }
    Ok(Some(req))
}

fn query<'a>(q: &'a str, key: &str) -> Option<&'a str> {
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

async fn write_payload<W: AsyncWrite + Unpin>(
    w: &mut W,
    seed: u64,
    n: usize,
    chunked: Option<usize>,
) -> std::io::Result<()> {
    let step = chunked.unwrap_or(64 * 1024).max(1);
    let mut buf = vec![0u8; step];
    let mut off = 0usize;
    while off < n {
        let k = step.min(n - off);
        payload::fill(seed, off as u64, &mut buf[..k]);
        if chunked.is_some() {
            w.write_all(format!("{k:x}\r\n").as_bytes()).await?;
            w.write_all(&buf[..k]).await?;
            w.write_all(b"\r\n").await?;
        } else {
            w.write_all(&buf[..k]).await?;
        }
        off += k;
    }
    Ok(())
}

async fn serve(s: TcpStream) {
    let id = CONN_ID.fetch_add(1, Ordering::Relaxed);
    let mut w = Wire::new(s);
    let mut seq = 0u64;
    loop {
        let req = match read_request(&mut w).await {
            Ok(Some(r)) => r,
            _ => return,
        };
        seq += 1;
        match respond(&mut w, &req, id, seq).await {
            Ok(true) => {}
            _ => return,
        }
    }
}

/// Answers one request; Ok(false) = connection closed afterwards.
async fn respond(
    w: &mut Wire<TcpStream>,
    req: &Request,
    id: u64,
    seq: u64,
) -> std::io::Result<bool> {
    // Target may be origin-form or absolute-form.
    let target = req
        .target
        .strip_prefix("http://")
        .map(|t| t.find('/').map_or("/", |i| &t[i..]))
        .unwrap_or(&req.target);
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let mut segs = path.trim_start_matches('/').split('/');
    let route = segs.next().unwrap_or("");
    let arg: usize = segs.next().and_then(|a| a.parse().ok()).unwrap_or(0);
    let head_only = req.method == "HEAD";
    if let Some(k) = query(q, "key") {
        hit(k);
    }
    let tag = format!("X-Conn: {id}\r\nX-Seq: {seq}\r\n");
    let s = &mut w.s;
    match route {
        "data" => {
            let seed = query(q, "seed")
                .and_then(|v| v.parse().ok())
                .unwrap_or(arg as u64);
            let framing = query(q, "framing").unwrap_or("cl");
            let chunk = query(q, "chunk")
                .and_then(|v| v.parse().ok())
                .unwrap_or(16 * 1024);
            let trailers = query(q, "trailers") == Some("1");
            match framing {
                "chunked" => {
                    let tr = if trailers { "Trailer: X-Sum\r\n" } else { "" };
                    s.write_all(
                        format!("HTTP/1.1 200 OK\r\n{tag}Transfer-Encoding: chunked\r\n{tr}\r\n")
                            .as_bytes(),
                    )
                    .await?;
                    if !head_only {
                        write_payload(s, seed, arg, Some(chunk)).await?;
                        let end = if trailers {
                            "0\r\nX-Sum: done\r\n\r\n"
                        } else {
                            "0\r\n\r\n"
                        };
                        s.write_all(end.as_bytes()).await?;
                    }
                    Ok(true)
                }
                "eof" => {
                    s.write_all(
                        format!("HTTP/1.1 200 OK\r\n{tag}Connection: close\r\n\r\n").as_bytes(),
                    )
                    .await?;
                    if !head_only {
                        write_payload(s, seed, arg, None).await?;
                    }
                    s.shutdown().await?;
                    Ok(false)
                }
                _ => {
                    s.write_all(
                        format!("HTTP/1.1 200 OK\r\n{tag}Content-Length: {arg}\r\n\r\n").as_bytes(),
                    )
                    .await?;
                    if !head_only {
                        write_payload(s, seed, arg, None).await?;
                    }
                    Ok(true)
                }
            }
        }
        "status" => {
            let reason = match arg {
                204 => "No Content",
                304 => "Not Modified",
                _ => "Status",
            };
            let cl = if arg == 204 || arg == 304 || arg < 200 {
                ""
            } else {
                "Content-Length: 0\r\n"
            };
            s.write_all(format!("HTTP/1.1 {arg} {reason}\r\n{tag}{cl}\r\n").as_bytes())
                .await?;
            Ok(true)
        }
        "1xx" => {
            s.write_all(b"HTTP/1.1 103 Early Hints\r\nLink: </style.css>; rel=preload\r\n\r\n")
                .await?;
            s.write_all(format!("HTTP/1.1 200 OK\r\n{tag}Content-Length: 2\r\n\r\nok").as_bytes())
                .await?;
            Ok(true)
        }
        "reset" => {
            s.write_all(
                format!("HTTP/1.1 200 OK\r\n{tag}Content-Length: {arg}\r\n\r\n").as_bytes(),
            )
            .await?;
            write_payload(s, arg as u64, arg / 2, None).await?;
            s.flush().await?;
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = socket2::SockRef::from(&*s).set_linger(Some(Duration::ZERO));
            Ok(false)
        }
        "drop" => Ok(false),
        "hits" => {
            let n = hits(query(q, "of").unwrap_or("")).to_string();
            s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\n{tag}Content-Length: {}\r\n\r\n{n}",
                    n.len()
                )
                .as_bytes(),
            )
            .await?;
            Ok(true)
        }
        "hang" => {
            let mut sink = [0u8; 1024];
            while matches!(s.read(&mut sink).await, Ok(n) if n > 0) {}
            Ok(false)
        }
        "close-idle" => {
            s.write_all(format!("HTTP/1.1 200 OK\r\n{tag}Content-Length: 2\r\n\r\nok").as_bytes())
                .await?;
            // Next request within 100 ms: served; otherwise close, leaving
            // the proxy a dead pooled socket.
            match tokio::time::timeout(Duration::from_millis(100), w.fill()).await {
                Ok(Ok(true)) => Ok(true),
                _ => Ok(false),
            }
        }
        "close" => {
            s.write_all(
                format!("HTTP/1.1 200 OK\r\n{tag}Content-Length: 2\r\nConnection: close\r\n\r\nok")
                    .as_bytes(),
            )
            .await?;
            s.shutdown().await?;
            Ok(false)
        }
        "echo" => {
            let mut body = req.head.clone();
            body.extend_from_slice(b"\r\n\r\n");
            body.extend_from_slice(&req.body);
            s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\n{tag}X-Req-Framing: {}\r\nContent-Length: {}\r\n\r\n",
                    req.framing,
                    body.len()
                )
                .as_bytes(),
            )
            .await?;
            if !head_only {
                s.write_all(&body).await?;
            }
            Ok(true)
        }
        "headers" => {
            let big = "a".repeat(arg);
            s.write_all(
                format!("HTTP/1.1 200 OK\r\n{tag}X-Big: {big}\r\nContent-Length: 2\r\n\r\nok")
                    .as_bytes(),
            )
            .await?;
            Ok(true)
        }
        "slow" => {
            let ms = query(q, "ms").and_then(|v| v.parse().ok()).unwrap_or(50);
            s.write_all(
                format!("HTTP/1.1 200 OK\r\n{tag}Content-Length: {arg}\r\n\r\n").as_bytes(),
            )
            .await?;
            let piece = arg.div_ceil(10).max(1);
            let mut off = 0;
            while off < arg {
                let k = piece.min(arg - off);
                let mut b = vec![0u8; k];
                payload::fill(arg as u64, off as u64, &mut b);
                s.write_all(&b).await?;
                off += k;
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            Ok(true)
        }
        "ws" => {
            let upgrade = req
                .header("upgrade")
                .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
                && req
                    .header("connection")
                    .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));
            let mode = query(q, "mode").unwrap_or("echo");
            if !upgrade || mode == "refuse" {
                s.write_all(
                    format!("HTTP/1.1 426 Upgrade Required\r\n{tag}Content-Length: 4\r\n\r\nnope")
                        .as_bytes(),
                )
                .await?;
                return Ok(true);
            }
            let mut first = format!(
                "HTTP/1.1 101 Switching Protocols\r\n{tag}Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: x\r\n\r\n"
            )
            .into_bytes();
            match mode {
                "head" => {
                    first.extend_from_slice(&req.head);
                    first.extend_from_slice(b"\r\n\r\n");
                }
                "early" => first.extend_from_slice(b"early-frame"),
                _ => {}
            }
            s.write_all(&first).await?;
            // Echo; bytes the client sent behind its handshake come back first.
            let pending = std::mem::take(&mut w.buf);
            if !pending.is_empty() {
                s.write_all(&pending).await?;
            }
            let mut b = [0u8; 4096];
            while let Ok(n) = s.read(&mut b).await {
                if n == 0 {
                    break;
                }
                s.write_all(&b[..n]).await?;
            }
            Ok(false)
        }
        _ => {
            s.write_all(
                format!("HTTP/1.1 404 Not Found\r\n{tag}Content-Length: 0\r\n\r\n").as_bytes(),
            )
            .await?;
            Ok(true)
        }
    }
}
