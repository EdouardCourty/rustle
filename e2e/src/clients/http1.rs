//! HTTP/1.1 client over plain TCP or TLS (the proxy's two HTTP ports).

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use tokio::io::AsyncWriteExt;

use super::{basic, BoxIo};
use crate::harness::certs;
use crate::harness::origin::{parse_headers, Wire};
use crate::harness::proxy::KEY;

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub trailers: Vec<String>,
    /// Interim (1xx) statuses received before the final one.
    pub interim: Vec<u16>,
    /// Body delimited by the connection close.
    pub eof_body: bool,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }

    pub fn expect(&self, status: u16) -> anyhow::Result<&Self> {
        anyhow::ensure!(
            self.status == status,
            "status {} != expected {status} (headers {:?})",
            self.status,
            self.headers
        );
        Ok(self)
    }
}

pub struct Client {
    pub w: Wire<BoxIo>,
}

impl Client {
    pub async fn plain(addr: SocketAddr) -> anyhow::Result<Self> {
        let s = tokio::net::TcpStream::connect(addr)
            .await
            .with_context(|| format!("connect {addr}"))?;
        s.set_nodelay(true)?;
        Ok(Self::over(Box::new(s)))
    }

    /// HTTPS proxy port, HTTP/1.1 (no ALPN, or `http/1.1` offered).
    pub async fn tls(addr: SocketAddr, offer_alpn: bool) -> anyhow::Result<Self> {
        let alpn: &[&[u8]] = if offer_alpn { &[b"http/1.1"] } else { &[] };
        let t = super::tls_connect(addr, certs::client(alpn)).await?;
        Ok(Self::over(Box::new(t)))
    }

    pub fn over(s: BoxIo) -> Self {
        Self { w: Wire::new(s) }
    }

    pub async fn send(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.w.s.write_all(bytes).await?;
        self.w.s.flush().await?;
        Ok(())
    }

    /// Reads one final response (1xx collected), body decoded.
    pub async fn response(&mut self, head_request: bool) -> anyhow::Result<Response> {
        tokio::time::timeout(Duration::from_secs(60), self.read_response(head_request))
            .await
            .context("response timeout")?
    }

    async fn read_response(&mut self, head_request: bool) -> anyhow::Result<Response> {
        let mut interim = Vec::new();
        loop {
            let head = self
                .w
                .head()
                .await
                .context("reading response head")?
                .context("connection closed before a response")?;
            let (first, headers) = parse_headers(&head);
            let status: u16 = first
                .get(9..12)
                .and_then(|c| c.parse().ok())
                .with_context(|| format!("bad status line {first:?}"))?;
            if (100..200).contains(&status) {
                interim.push(status);
                continue;
            }
            let mut r = Response {
                status,
                headers,
                body: Vec::new(),
                trailers: Vec::new(),
                interim,
                eof_body: false,
            };
            if head_request || status == 204 || status == 304 {
                return Ok(r);
            }
            if r.header("transfer-encoding")
                .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
            {
                let (b, t) = self.w.chunked().await.context("chunked body")?;
                r.body = b;
                r.trailers = t;
            } else if let Some(n) = r.header("content-length") {
                let n: usize = n.parse().context("content-length")?;
                r.body = self.w.exact(n).await.context("content-length body")?;
            } else {
                r.body = self
                    .w
                    .read_to_close()
                    .await
                    .unwrap_or_else(|_| std::mem::take(&mut self.w.buf));
                r.eof_body = true;
            }
            return Ok(r);
        }
    }

    pub async fn roundtrip(&mut self, req: &[u8], head_request: bool) -> anyhow::Result<Response> {
        self.send(req).await?;
        self.response(head_request).await
    }

    /// CONNECT on this connection; returns the status (200 = tunnel up,
    /// the connection is then raw bytes).
    pub async fn connect(&mut self, authority: &str, auth: Option<&str>) -> anyhow::Result<u16> {
        self.send(connect_req(authority, auth).as_bytes()).await?;
        let head = self
            .w
            .head()
            .await?
            .context("closed before the CONNECT response")?;
        let (first, _) = parse_headers(&head);
        first
            .get(9..12)
            .and_then(|c| c.parse().ok())
            .with_context(|| format!("bad status line {first:?}"))
    }

    /// The raw stream (after a successful CONNECT). Bytes the target sent
    /// along with the CONNECT response (a server that speaks first) are
    /// read first.
    pub fn into_stream(self) -> anyhow::Result<BoxIo> {
        if self.w.buf.is_empty() {
            return Ok(self.w.s);
        }
        Ok(Box::new(Prefixed {
            pending: self.w.buf,
            inner: self.w.s,
        }))
    }

    /// True if the proxy closes the connection within `t`.
    pub async fn closed(&mut self, t: Duration) -> bool {
        if !self.w.buf.is_empty() {
            return false;
        }
        super::closed_within(&mut self.w.s, t).await
    }
}

pub fn auth_header(auth: Option<&str>) -> String {
    match auth {
        Some(v) => format!("Proxy-Authorization: {v}\r\n"),
        None => String::new(),
    }
}

/// Default credentials (user `u`, first API key).
pub fn default_auth() -> String {
    basic("u", KEY)
}

pub fn connect_req(authority: &str, auth: Option<&str>) -> String {
    format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{}\r\n",
        auth_header(auth)
    )
}

/// Absolute-form request with optional body (Content-Length).
pub fn request(method: &str, url: &str, auth: Option<&str>, extra: &str, body: &[u8]) -> Vec<u8> {
    let host = url
        .strip_prefix("http://")
        .and_then(|u| u.split('/').next())
        .unwrap_or("");
    let cl = if body.is_empty() && method != "POST" && method != "PUT" {
        String::new()
    } else {
        format!("Content-Length: {}\r\n", body.len())
    };
    let mut r = format!(
        "{method} {url} HTTP/1.1\r\nHost: {host}\r\n{}{extra}{cl}\r\n",
        auth_header(auth)
    )
    .into_bytes();
    r.extend_from_slice(body);
    r
}

pub fn get(url: &str) -> Vec<u8> {
    request("GET", url, Some(&default_auth()), "", b"")
}

/// Chunked encoding of `body` in `chunk`-byte pieces (+ optional trailer).
pub fn chunked(body: &[u8], chunk: usize, trailer: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    for c in body.chunks(chunk.max(1)) {
        out.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
        out.extend_from_slice(c);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n");
    if let Some(t) = trailer {
        out.extend_from_slice(format!("{t}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// A stream whose first reads return `pending`.
struct Prefixed {
    pending: Vec<u8>,
    inner: BoxIo,
}

impl tokio::io::AsyncRead for Prefixed {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if !self.pending.is_empty() {
            let n = self.pending.len().min(buf.remaining());
            buf.put_slice(&self.pending[..n]);
            self.pending.drain(..n);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for Prefixed {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
