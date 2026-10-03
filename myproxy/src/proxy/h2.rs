//! HTTP/2 on the HTTPS proxy listener (ALPN `h2`).
//!
//! One h2 connection per client, one task per stream (same shard, same
//! upstream pool as the HTTP/1 path):
//! - `CONNECT authority`: the stream becomes a TCP tunnel (h2 DATA frames
//!   <-> upstream socket, h2 flow control honoured both ways);
//! - other methods (`:scheme http`): forwarded as HTTP/1.1 to the origin
//!   through the pool; the response is re-framed as h2 (chunked bodies are
//!   decoded — h2 forbids transfer codings).
//!
//! Same rules as HTTP/1: auth checked per request, request/response body
//! budgets, hop-by-hop headers dropped, stale pooled sockets retried once
//! for bodiless requests.

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use h2::server::SendResponse;
use h2::{Reason, RecvStream, SendStream};
use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::pipeline::{self, UpstreamError};
use crate::accesslog::TargetMode;
use crate::config::Config;
use crate::http::{read_response_head, HeadError, RespError};
use crate::relay::{chunk_size, IdleTimer};

/// Streams in flight per client connection.
const MAX_CONCURRENT_STREAMS: u32 = 256;
/// h2 flow-control windows: per stream, and per connection.
const STREAM_WINDOW: u32 = 4 << 20;
const CONN_WINDOW: u32 = 16 << 20;
/// Largest DATA payload handed to h2 at once (it splits into frames).
const DATA_CHUNK: usize = 64 * 1024;
/// Largest frame the client may send us (SETTINGS_MAX_FRAME_SIZE; the 16 KB
/// default cut uploads into 16 KB frames = one upstream write each; 1 MB
/// is Go's net/http2 server default).
const MAX_FRAME: u32 = 1 << 20;

/// Request headers never forwarded upstream: hop-by-hop, proxy-only, and
/// the ones the HTTP/1.1 head is rebuilt with.
const DROP_REQ: &[&str] = &[
    "connection",
    "proxy-connection",
    "keep-alive",
    "te",
    "transfer-encoding",
    "upgrade",
    "proxy-authorization",
    "host",
    "content-length",
];
/// Response headers that are HTTP/1 connection-specific (illegal in h2).
const DROP_RESP: &[&str] = &[
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "upgrade",
];

/// The shard's services plus the client address (auth failures).
struct Ctx {
    svc: Arc<super::Services>,
    peer: IpAddr,
}

impl std::ops::Deref for Ctx {
    type Target = super::Services;
    fn deref(&self) -> &super::Services {
        &self.svc
    }
}

/// Decrements the connection's live-stream count when a stream task ends.
struct StreamSlot(Arc<AtomicUsize>);

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

pub async fn handle_h2<T>(client: T, peer: IpAddr, svc: Arc<super::Services>)
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let cfg = svc.cfg.clone();
    let idle = cfg.timeouts.idle;
    let mut builder = h2::server::Builder::new();
    builder
        .max_concurrent_streams(MAX_CONCURRENT_STREAMS)
        .max_header_list_size(cfg.limits.max_header_bytes as u32)
        .initial_window_size(STREAM_WINDOW)
        .initial_connection_window_size(CONN_WINDOW)
        .max_frame_size(MAX_FRAME);
    let mut conn = match timeout(
        cfg.tls.handshake_timeout,
        builder.handshake::<_, Bytes>(client),
    )
    .await
    {
        Ok(Ok(c)) => c,
        _ => return,
    };
    let ctx = Arc::new(Ctx { svc, peer });
    let live = Arc::new(AtomicUsize::new(0));
    loop {
        tokio::select! {
            next = conn.accept() => match next {
                Some(Ok((req, respond))) => {
                    live.fetch_add(1, Relaxed);
                    let slot = StreamSlot(live.clone());
                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        let _slot = slot;
                        serve_stream(&ctx, req, respond).await;
                    });
                }
                Some(Err(e)) => {
                    tracing::trace!(error = %e, "h2 connection error");
                    return;
                }
                None => return,
            },
            // Idle connection (no stream in flight for `idle`): go away.
            _ = tokio::time::sleep(idle) => {
                if live.load(Relaxed) == 0 {
                    conn.graceful_shutdown();
                    let _ = timeout(
                        Duration::from_secs(1),
                        std::future::poll_fn(|cx| conn.poll_closed(cx)),
                    )
                    .await;
                    return;
                }
            }
        }
    }
}

fn reply(respond: &mut SendResponse<Bytes>, status: StatusCode) {
    let mut resp = Response::new(());
    *resp.status_mut() = status;
    if status == StatusCode::PROXY_AUTHENTICATION_REQUIRED {
        resp.headers_mut().insert(
            http::header::PROXY_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"myproxy\""),
        );
    }
    let _ = respond.send_response(resp, true);
}

async fn serve_stream(ctx: &Ctx, req: Request<RecvStream>, mut respond: SendResponse<Bytes>) {
    let auth = req
        .headers()
        .get(http::header::PROXY_AUTHORIZATION)
        .map(|v| v.as_bytes());
    if !ctx.auth.check(auth) {
        if auth.is_some() {
            ctx.svc.auth_failed(ctx.peer, "h2");
        }
        reply(&mut respond, StatusCode::PROXY_AUTHENTICATION_REQUIRED);
        return;
    }
    if req.method() == Method::CONNECT {
        connect(ctx, req, respond).await;
    } else {
        forward(ctx, req, respond).await;
    }
}

/// `host:port` of a request authority (brackets stripped from IPv6).
fn target(req: &Request<RecvStream>, default_port: u16) -> Option<(String, u16)> {
    let a = req.uri().authority()?;
    let host = a.host().trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), a.port_u16().unwrap_or(default_port)))
}

/// Error = (status for the client, refusal reason for the access log).
async fn dial(
    ctx: &Ctx,
    host: &str,
    port: u16,
    key: &str,
) -> Result<TcpStream, (StatusCode, Option<&'static str>)> {
    match pipeline::connect_upstream(
        host,
        port,
        ctx.cfg.timeouts.connect,
        &ctx.dns,
        key,
        &ctx.dest,
    )
    .await
    {
        Ok(up) => {
            let _ = up.set_nodelay(true);
            Ok(up)
        }
        Err(UpstreamError::Timeout) => Err((StatusCode::GATEWAY_TIMEOUT, None)),
        Err(UpstreamError::Forbidden(r)) => Err((StatusCode::FORBIDDEN, Some(r))),
        Err(UpstreamError::Other(e)) => {
            tracing::debug!(error = %e, "upstream connect failed");
            Err((StatusCode::BAD_GATEWAY, None))
        }
    }
}

fn origin_key(host: &str, port: u16) -> String {
    let mut key = String::with_capacity(host.len() + 6);
    key.push_str(host);
    key.push(':');
    pipeline::push_u16(&mut key, port);
    key
}

// ---- CONNECT tunnel -----------------------------------------------------

async fn connect(ctx: &Ctx, req: Request<RecvStream>, mut respond: SendResponse<Bytes>) {
    let Some((host, port)) = target(&req, 443) else {
        ctx.svc.log_fail(ctx.peer, "h2", "connect", "-", 400, None);
        reply(&mut respond, StatusCode::BAD_REQUEST);
        return;
    };
    let key = origin_key(&host, port);
    let up = match dial(ctx, &host, port, &key).await {
        Ok(up) => up,
        Err((status, reason)) => {
            ctx.svc
                .log_fail(ctx.peer, "h2", "connect", &key, status.as_u16(), reason);
            reply(&mut respond, status);
            return;
        }
    };
    let Ok(send) = respond.send_response(Response::new(()), false) else {
        return;
    };
    let t0 = std::time::Instant::now();
    let (c2u, u2c) = tunnel(req.into_body(), send, up, ctx.cfg.timeouts.idle).await;
    tracing::trace!(c2u, u2c, "h2 tunnel closed");
    if ctx.log.all() {
        ctx.svc
            .log_tunnel(ctx.peer, "h2", "connect", &key, (c2u, u2c), t0);
    }
}

/// Both directions of an h2 tunnel; half-close propagated (END_STREAM ->
/// shutdown(WR), upstream EOF -> END_STREAM).
async fn tunnel(
    mut body: RecvStream,
    mut send: SendStream<Bytes>,
    up: TcpStream,
    idle: Duration,
) -> (u64, u64) {
    let (mut ur, mut uw) = up.into_split();
    let c2u = async {
        let mut t = IdleTimer::new(idle);
        let mut n: u64 = 0;
        while let Some(Some(Ok(chunk))) = t.park(body.data()).await {
            let len = chunk.len();
            // Reopen the window as soon as the frame is ours (not after the
            // upstream write): the client keeps sending while we write.
            // Still bounded: h2 never buffers more than the window.
            let _ = body.flow_control().release_capacity(len);
            if !matches!(t.park(uw.write_all(&chunk)).await, Some(Ok(()))) {
                break;
            }
            n += len as u64;
            t.reset();
        }
        let _ = uw.shutdown().await;
        n
    };
    let u2c = async {
        let mut t = IdleTimer::new(idle);
        let mut n: u64 = 0;
        let mut buf = BytesMut::with_capacity(DATA_CHUNK);
        loop {
            buf.reserve(DATA_CHUNK);
            match t.park(ur.read_buf(&mut buf)).await {
                Some(Ok(0)) => {
                    let _ = send.send_data(Bytes::new(), true);
                    break;
                }
                Some(Ok(k)) => {
                    if !send_all(&mut send, buf.split().freeze(), &mut t).await {
                        break;
                    }
                    n += k as u64;
                    t.reset();
                }
                _ => {
                    send.send_reset(Reason::CANCEL);
                    break;
                }
            }
        }
        n
    };
    tokio::join!(c2u, u2c)
}

/// Sends `data` within the stream's flow-control window (waits for window
/// updates). `false` when the stream is gone or idle.
async fn send_all(send: &mut SendStream<Bytes>, mut data: Bytes, t: &mut IdleTimer) -> bool {
    while !data.is_empty() {
        send.reserve_capacity(data.len());
        let cap = match t
            .park(std::future::poll_fn(|cx| send.poll_capacity(cx)))
            .await
        {
            Some(Some(Ok(c))) => c,
            _ => return false,
        };
        if cap == 0 {
            continue;
        }
        let chunk = data.split_to(cap.min(data.len()));
        if send.send_data(chunk, false).is_err() {
            return false;
        }
    }
    true
}

// ---- plain HTTP forward -------------------------------------------------

/// HTTP/1.1 request head for the origin (origin-form, rebuilt headers).
/// `body`: `None` = no body, `Some(Some(n))` = Content-Length n,
/// `Some(None)` = chunked.
fn request_head(req: &Request<RecvStream>, authority: &str, body: Option<Option<u64>>) -> Vec<u8> {
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let mut h = Vec::with_capacity(256);
    h.extend_from_slice(req.method().as_str().as_bytes());
    h.push(b' ');
    h.extend_from_slice(if path.is_empty() {
        b"/"
    } else {
        path.as_bytes()
    });
    h.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    h.extend_from_slice(authority.as_bytes());
    h.extend_from_slice(b"\r\n");
    for (name, value) in req.headers() {
        if DROP_REQ.contains(&name.as_str()) {
            continue;
        }
        h.extend_from_slice(name.as_str().as_bytes());
        h.extend_from_slice(b": ");
        h.extend_from_slice(value.as_bytes());
        h.extend_from_slice(b"\r\n");
    }
    match body {
        Some(Some(cl)) => h.extend_from_slice(format!("Content-Length: {cl}\r\n").as_bytes()),
        Some(None) => h.extend_from_slice(b"Transfer-Encoding: chunked\r\n"),
        None => {}
    }
    h.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    h
}

async fn forward(ctx: &Ctx, req: Request<RecvStream>, mut respond: SendResponse<Bytes>) {
    let cfg = &ctx.cfg;
    let idle = cfg.timeouts.idle;
    let t0 = ctx.log.all().then(std::time::Instant::now);
    let method = req.method().clone();
    // Built only when a line can be written (no allocation per stream
    // with the access log off).
    let log_target = ctx.log.errors().then(|| match ctx.log.target_mode() {
        TargetMode::Url => req.uri().to_string(),
        TargetMode::Host => req
            .uri()
            .authority()
            .map(|a| a.as_str().to_string())
            .unwrap_or_else(|| "-".into()),
    });
    // Failure: access-log line + status to the client.
    let fail = |respond: &mut SendResponse<Bytes>, status: StatusCode, reason: Option<&str>| {
        if ctx.log.errors() {
            let mut l = ctx
                .log
                .line()
                .kv("client", ctx.peer)
                .kv("via", "h2")
                .kv("kind", "http")
                .kv("method", &method)
                .kv("target", log_target.as_deref().unwrap_or("-"))
                .kv("status", status.as_u16());
            if let Some(r) = reason {
                l = l.kv("reason", r);
            }
            ctx.log.emit(l);
        }
        reply(respond, status);
    };
    if req.uri().scheme_str() != Some("http") {
        fail(&mut respond, StatusCode::BAD_REQUEST, Some("scheme"));
        return;
    }
    let Some((host, port)) = target(&req, 80) else {
        fail(&mut respond, StatusCode::BAD_REQUEST, None);
        return;
    };
    let authority = req
        .uri()
        .authority()
        .map(|a| a.as_str().to_string())
        .unwrap_or_default();
    let key = origin_key(&host, port);

    // Request body framing: h2 frames the body itself; HTTP/1.1 upstream
    // needs a length (no chunked uploads — same rule as the HTTP/1 path).
    let declared = match req.headers().get(http::header::CONTENT_LENGTH) {
        None => None,
        Some(v) => match v.to_str().ok().and_then(|s| s.trim().parse::<u64>().ok()) {
            Some(n) => Some(n),
            None => {
                fail(
                    &mut respond,
                    StatusCode::BAD_REQUEST,
                    Some("content-length"),
                );
                return;
            }
        },
    };
    // A body without Content-Length is streamed upstream as chunked.
    let has_body = !req.body().is_end_stream();
    let body_len = if has_body { declared } else { Some(0) };
    let budget = cfg.http.max_request_body_bytes;
    if budget > 0 && body_len.is_some_and(|n| n > budget) {
        fail(&mut respond, StatusCode::PAYLOAD_TOO_LARGE, None);
        return;
    }
    let head = request_head(&req, &authority, has_body.then_some(body_len));
    let method_head = req.method() == Method::HEAD;
    // B7: only an idempotent request without body may be replayed on a
    // fresh connection (same rule as the HTTP/1 pipeline).
    let replayable = !has_body && (method_head || req.method() == Method::GET);
    let mut body = req.into_body();

    // Upstream: pooled socket (one retry on a stale one when the request
    // is replayable) or a fresh dial.
    let mut resp_buf: Vec<u8> = Vec::new();
    let mut attempt = 0;
    let mut sent: u64 = 0;
    let (mut up, rhead) = loop {
        attempt += 1;
        let (mut up, pooled) = match ctx.pool.take(&key) {
            Some(s) => (s, true),
            None => match dial(ctx, &host, port, &key).await {
                Ok(s) => (s, false),
                Err((status, reason)) => {
                    fail(&mut respond, status, reason);
                    return;
                }
            },
        };
        if timeout(idle, up.write_all(&head))
            .await
            .map(|r| r.is_err())
            .unwrap_or(true)
        {
            if pooled && attempt == 1 && replayable {
                continue;
            }
            fail(&mut respond, StatusCode::BAD_GATEWAY, None);
            return;
        }
        if has_body {
            match upload(&mut body, &mut up, body_len, budget, idle).await {
                Some(n) => sent = n,
                None => {
                    respond.send_reset(Reason::CANCEL);
                    return;
                }
            }
        }
        match read_head_final(&mut up, &mut resp_buf, cfg, idle).await {
            Ok(h) => break (up, h),
            Err(RespError::Head(HeadError::Io(e))) if e.kind() == std::io::ErrorKind::TimedOut => {
                fail(
                    &mut respond,
                    StatusCode::GATEWAY_TIMEOUT,
                    Some("no-response"),
                );
                return;
            }
            Err(RespError::Closed) | Err(RespError::Head(HeadError::Io(_)))
                if pooled && attempt == 1 && replayable =>
            {
                continue
            }
            Err(_) => {
                fail(&mut respond, StatusCode::BAD_GATEWAY, Some("no-response"));
                return;
            }
        }
    };

    // Response head -> h2 headers.
    let mut resp = Response::new(());
    *resp.status_mut() = StatusCode::from_u16(rhead.status).unwrap_or(StatusCode::BAD_GATEWAY);
    {
        let mut headers = [httparse::EMPTY_HEADER; crate::http::MAX_HEADERS];
        let mut r = httparse::Response::new(&mut headers);
        let _ = r.parse(&resp_buf[..rhead.raw_len]);
        for h in r.headers.iter() {
            let lower = h.name.to_ascii_lowercase();
            if DROP_RESP.contains(&lower.as_str()) || (rhead.chunked && lower == "content-length") {
                continue;
            }
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(lower.as_bytes()),
                HeaderValue::from_bytes(h.value),
            ) {
                resp.headers_mut().append(n, v);
            }
        }
    }

    let budget = cfg.http.max_response_body_bytes;
    let zero_body = rhead.no_body_status || method_head;
    if !zero_body && budget > 0 && rhead.content_length.is_some_and(|l| l > budget) {
        fail(&mut respond, StatusCode::BAD_GATEWAY, Some("over-budget"));
        return;
    }
    let Ok(mut send) = respond.send_response(resp, zero_body) else {
        return;
    };
    // Success line (all level), with the response body bytes relayed.
    let log_ok = |down: u64| {
        if let Some(t0) = t0 {
            ctx.log.emit(
                ctx.log
                    .line()
                    .kv("client", ctx.peer)
                    .kv("via", "h2")
                    .kv("kind", "http")
                    .kv("method", &method)
                    .kv("target", log_target.as_deref().unwrap_or("-"))
                    .kv("status", rhead.status)
                    .kv("up", sent)
                    .kv("down", down)
                    .kv("ms", t0.elapsed().as_millis()),
            );
        }
    };
    if zero_body {
        log_ok(0);
        if rhead.keep_alive && resp_buf.len() == rhead.raw_len {
            ctx.pool.put(&key, up);
        }
        return;
    }

    let mut src = Src {
        up: &mut up,
        buf: BytesMut::from(&resp_buf[rhead.raw_len..]),
        timer: IdleTimer::new(idle),
        given: 0,
    };
    let result = if rhead.chunked {
        dechunk(&mut src, &mut send, budget).await
    } else if let Some(len) = rhead.content_length {
        sized(&mut src, &mut send, len).await
    } else {
        until_eof(&mut src, &mut send, budget).await
    };
    let clean = match result {
        Ok(()) => {
            let _ = send.send_data(Bytes::new(), true);
            log_ok(src.given);
            !src.leftover()
        }
        Err(e) => {
            tracing::debug!(upstream = %key, error = %e, "h2 response relay aborted");
            send.send_reset(Reason::INTERNAL_ERROR);
            if ctx.log.errors() {
                ctx.log.emit(
                    ctx.log
                        .line()
                        .kv("client", ctx.peer)
                        .kv("via", "h2")
                        .kv("kind", "http")
                        .kv("method", &method)
                        .kv("target", log_target.as_deref().unwrap_or("-"))
                        .kv("status", rhead.status)
                        .kv("reason", "truncated"),
                );
            }
            false
        }
    };
    // Framed, complete, nothing over-read, keep-alive: reusable.
    if clean && rhead.keep_alive && (rhead.chunked || rhead.content_length.is_some()) {
        ctx.pool.put(&key, up);
    }
}

/// Streams the h2 request body upstream: exactly `len` bytes, or chunked
/// framing when the length is unknown (each DATA frame = one chunk).
/// `budget` caps the body (0 = unlimited).
async fn upload(
    body: &mut RecvStream,
    up: &mut TcpStream,
    len: Option<u64>,
    budget: u64,
    idle: Duration,
) -> Option<u64> {
    let mut t = IdleTimer::new(idle);
    let mut sent: u64 = 0;
    loop {
        match t.park(body.data()).await {
            Some(Some(Ok(chunk))) => {
                sent += chunk.len() as u64;
                if len.is_some_and(|l| sent > l) || (budget > 0 && sent > budget) {
                    return None;
                }
                let n = chunk.len();
                let ok = match len {
                    Some(_) => matches!(t.park(up.write_all(&chunk)).await, Some(Ok(()))),
                    None if n == 0 => true,
                    None => {
                        // Size line + data + CRLF: one small head alloc per
                        // frame, then a single buffered write sequence.
                        let size = format!("{n:x}\r\n");
                        let mut framed = Vec::with_capacity(size.len() + n + 2);
                        framed.extend_from_slice(size.as_bytes());
                        framed.extend_from_slice(&chunk);
                        framed.extend_from_slice(b"\r\n");
                        matches!(t.park(up.write_all(&framed)).await, Some(Ok(())))
                    }
                };
                if !ok {
                    return None;
                }
                let _ = body.flow_control().release_capacity(n);
                t.reset();
            }
            Some(None) => {
                let ok = match len {
                    Some(l) => sent == l,
                    None => matches!(t.park(up.write_all(b"0\r\n\r\n")).await, Some(Ok(()))),
                };
                return ok.then_some(sent);
            }
            _ => return None,
        }
    }
}

/// Reads the final response head (1xx informational heads are skipped:
/// h2 clients get the final one).
async fn read_head_final(
    up: &mut TcpStream,
    resp_buf: &mut Vec<u8>,
    cfg: &Config,
    idle: Duration,
) -> Result<crate::http::ResponseHead, RespError> {
    let max = cfg.limits.max_header_bytes;
    // The idle timeout is applied per read inside read_response_head.
    let mut h = read_response_head(up, resp_buf, max, idle, &[]).await?;
    for _ in 0..3 {
        if !(100..200).contains(&h.status) || h.status == 101 {
            return Ok(h);
        }
        let rest = resp_buf[h.raw_len..].to_vec();
        h = read_response_head(up, resp_buf, max, idle, &rest).await?;
    }
    Err(RespError::Head(HeadError::Invalid))
}

/// Upstream bytes: what was read along with the head, then the socket.
struct Src<'a> {
    up: &'a mut TcpStream,
    /// Unconsumed upstream bytes. Chunks handed to h2 are split off this
    /// buffer (`split_to().freeze()`): no copy per DATA frame.
    buf: BytesMut,
    timer: IdleTimer,
    /// Body bytes handed to the client (access log).
    given: u64,
}

impl Src<'_> {
    async fn fill(&mut self) -> std::io::Result<()> {
        self.buf.reserve(DATA_CHUNK);
        match self.timer.park(self.up.read_buf(&mut self.buf)).await {
            Some(Ok(0)) => Err(std::io::ErrorKind::UnexpectedEof.into()),
            Some(Ok(_)) => {
                self.timer.reset();
                Ok(())
            }
            Some(Err(e)) => Err(e),
            None => Err(std::io::ErrorKind::TimedOut.into()),
        }
    }

    /// Next line (through `\n`), at most `cap` bytes.
    async fn line(&mut self, cap: usize) -> std::io::Result<Vec<u8>> {
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                return Ok(self.buf.split_to(i + 1).to_vec());
            }
            if self.buf.len() > cap {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line too long",
                ));
            }
            self.fill().await?;
        }
    }

    /// Up to `max` bytes (reads when nothing is buffered).
    async fn take(&mut self, max: usize) -> std::io::Result<Bytes> {
        if self.buf.is_empty() {
            self.fill().await?;
        }
        let k = self.buf.len().min(max);
        self.given += k as u64;
        Ok(self.buf.split_to(k).freeze())
    }

    fn leftover(&self) -> bool {
        !self.buf.is_empty()
    }
}

fn gone() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "h2 stream gone")
}

async fn sized(src: &mut Src<'_>, send: &mut SendStream<Bytes>, len: u64) -> std::io::Result<()> {
    let mut left = len;
    while left > 0 {
        let b = src.take(left.min(DATA_CHUNK as u64) as usize).await?;
        left -= b.len() as u64;
        if !send_all(send, b, &mut src.timer).await {
            return Err(gone());
        }
    }
    Ok(())
}

async fn until_eof(
    src: &mut Src<'_>,
    send: &mut SendStream<Bytes>,
    budget: u64,
) -> std::io::Result<()> {
    let mut total: u64 = 0;
    loop {
        let b = match src.take(DATA_CHUNK).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        total += b.len() as u64;
        if budget > 0 && total > budget {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "response over budget",
            ));
        }
        if !send_all(send, b, &mut src.timer).await {
            return Err(gone());
        }
    }
}

/// Decodes a chunked body into h2 DATA frames (trailers are dropped).
async fn dechunk(
    src: &mut Src<'_>,
    send: &mut SendStream<Bytes>,
    budget: u64,
) -> std::io::Result<()> {
    let bad = |m| std::io::Error::new(std::io::ErrorKind::InvalidData, m);
    let mut total: u64 = 0;
    loop {
        let line = src.line(128).await?;
        let size = chunk_size(&line).ok_or_else(|| bad("bad chunk size"))?;
        if size == 0 {
            for _ in 0..=64 {
                if src.line(1024).await? == b"\r\n" {
                    return Ok(());
                }
            }
            return Err(bad("too many trailer lines"));
        }
        total = total.saturating_add(size);
        if budget > 0 && total > budget {
            return Err(bad("response over budget"));
        }
        let mut left = size;
        while left > 0 {
            let b = src.take(left.min(DATA_CHUNK as u64) as usize).await?;
            left -= b.len() as u64;
            if !send_all(send, b, &mut src.timer).await {
                return Err(gone());
            }
        }
        if src.line(2).await? != b"\r\n" {
            return Err(bad("bad chunk terminator"));
        }
    }
}
