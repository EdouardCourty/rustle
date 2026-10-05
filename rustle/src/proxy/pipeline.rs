//! The HTTP forward pipeline: upstream connection (pooled or fresh),
//! request forwarding, response head classification, body relay with exact
//! bounds, and pooling decisions. One function per logical request from
//! the FSM in [`super`].

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::accesslog::TargetMode;
use crate::config::Config;
use crate::dns::DnsCache;
use crate::http::{
    read_response_head, HeadError, RequestHead, RespError, ResponseHead, Span, Target,
};
use crate::policy::DestPolicy;
use crate::relay;
use crate::stream::ClientStream;

use super::{RESP_502, RESP_504};

pub(crate) enum UpstreamError {
    Timeout,
    /// Refused by the destination policy: "port" | "private" | "deny".
    Forbidden(&'static str),
    Other(std::io::Error),
}

/// 413 Payload Too Large (body limit exceeded) — one-line each way, the
/// client must know the request was rejected before it finishes sending
/// (the proxy closes write-up on the response to free the upload).
pub(crate) const RESP_413: &[u8] =
    b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Serves one HTTP request end-to-end (upstream fetch + body relay + pool
/// accounting). Returns `false` when the client connection must be dropped
/// (client asked to close, broken state); `true` keeps the keep-alive loop
/// alive. All error paths already wrote the error response to the client.
#[allow(clippy::too_many_arguments)]
pub(super) async fn http<C: ClientStream>(
    client: &mut C,
    svc: &super::Services,
    peer: std::net::IpAddr,
    buf: &mut [u8],
    head: &RequestHead,
    target: &Target,
    // Connection-owned scratches: zero alloc in the hot path — reused
    // across keep-alive requests, dropped with the connection.
    resp_buf: &mut Vec<u8>,
    rewrite_buf: &mut Vec<u8>,
    origin_key: &mut String,
) -> bool {
    let cfg: &Config = &svc.cfg;
    let pool = &svc.pool;
    let idle = cfg.timeouts.idle;
    let connect = cfg.timeouts.connect;
    let max_head = cfg.limits.max_header_bytes;
    // host/origin are spans into `buf`; the origin key is rebuilt per
    // request into the connection-owned scratch (port digits appended
    // without allocating).
    let (host_span, port, origin_span) = match target {
        Target::Http { host, port, origin } => (*host, *port, *origin),
        _ => return false,
    };
    let host_str = std::str::from_utf8(host_span.slice(buf)).unwrap_or("");
    origin_key.clear();
    origin_key.push_str(&String::from_utf8_lossy(host_span.slice(buf)));
    origin_key.push(':');
    push_u16(origin_key, port);
    let key: &str = origin_key;
    // Access log: start time only when every request is logged; the
    // target string is built only when a line is actually written.
    let t0 = svc.log.all().then(std::time::Instant::now);
    let method_bytes = head.method.slice(buf);
    let log_target = || match svc.log.target_mode() {
        TargetMode::Host => key.to_string(),
        TargetMode::Url => {
            let origin = origin_span.slice(buf);
            let origin = if origin.is_empty() { &b"/"[..] } else { origin };
            format!("http://{key}{}", String::from_utf8_lossy(origin))
        }
    };
    let log_fail = |status: u16, reason: Option<&str>| {
        if svc.log.errors() {
            let mut l = svc
                .log
                .line()
                .kv("client", peer)
                .kv("via", C::VIA)
                .kv("kind", "http")
                .kv_bytes("method", method_bytes)
                .kv("target", log_target())
                .kv("status", status);
            if let Some(r) = reason {
                l = l.kv("reason", r);
            }
            svc.log.emit(l);
        }
    };

    // Upstream connection: pooled reuse when possible, fresh otherwise. A
    // pooled connection that died while idle is retried once on a fresh
    // one (safe: no response byte has reached the client at that point).
    let mut attempt = 0;
    let mut close_after;
    let mut sent: u64;
    let (mut up, _from_pool, mut rhead) = loop {
        attempt += 1;
        let (mut up, from_pool) = match pool.take(key) {
            Some(s) => (s, true),
            None => match connect_upstream(host_str, port, connect, &svc.dns, key, &svc.dest).await
            {
                Ok(s) => (s, false),
                Err(UpstreamError::Forbidden(reason)) => {
                    log_fail(403, Some(reason));
                    super::reply(client, super::RESP_403).await;
                    return false;
                }
                Err(UpstreamError::Timeout) => {
                    log_fail(504, None);
                    super::reply(client, RESP_504).await;
                    return false;
                }
                Err(UpstreamError::Other(e)) => {
                    tracing::debug!(error = %e, "upstream connect failed");
                    log_fail(502, None);
                    super::reply(client, RESP_502).await;
                    return false;
                }
            },
        };
        // NODELAY is set once when the socket is fresh; pooled sockets keep
        // theirs (one saved syscall per request in the hot path).
        if !from_pool {
            let _ = up.set_nodelay(true);
        }

        // B7: a retry is only safe when the request was forwarded from the
        // already-buffered bytes (no client body still being drained) and
        // the method is idempotent — otherwise a retry could re-execute a
        // POST or resend a head without its body.
        let method = head.method.slice(buf);
        let idempotent =
            method.eq_ignore_ascii_case(b"GET") || method.eq_ignore_ascii_case(b"HEAD");
        let replayable = idempotent
            && !head.chunked
            && head.content_length.unwrap_or(0) as usize == buf.len().saturating_sub(head.raw_len);

        let mut ctx = UpWriteCtx {
            idle,
            max_request_body: cfg.http.max_request_body_bytes,
            body_rejected: false,
            bad_request: false,
            client_gone: false,
            close_after: false,
            sent: 0,
            rewrite_buf,
        };
        let forwarded = forward_request(client, &mut up, buf, head, origin_span, &mut ctx).await;
        close_after = ctx.close_after;
        sent = ctx.sent;
        if !forwarded {
            // Body over budget: answer 413 and drop (the client may still
            // be uploading — the close aborts it, like nginx does).
            if ctx.body_rejected {
                log_fail(413, None);
                super::reply(client, RESP_413).await;
                return false;
            }
            if ctx.bad_request {
                log_fail(400, Some("chunked"));
                super::reply(client, super::RESP_400).await;
                return false;
            }
            if ctx.client_gone {
                log_fail(400, Some("body-incomplete"));
                return false;
            }
            // Upstream write failed: a pooled socket that died while idle
            // is retried like a failed read (B6/B7).
            if from_pool && attempt == 1 && replayable {
                continue;
            }
            log_fail(502, Some("upstream-write"));
            super::reply(client, RESP_502).await;
            return false;
        }
        // The idle timeout is applied per read inside read_response_head.
        match split_timeout(read_response_head(&mut up, resp_buf, max_head, idle, &[]).await) {
            Ok(Ok(h)) => break (up, from_pool, h),
            // B6: retry once on ANY I/O failure of a pooled socket — clean
            // EOF or RST — as long as the request is replayable (B7).
            Ok(Err(RespError::Head(HeadError::Io(_))))
                if from_pool && attempt == 1 && replayable =>
            {
                continue; // stale/rst pooled socket: retry, fresh this time
            }
            Ok(Err(RespError::Closed)) if from_pool && attempt == 1 && replayable => {
                continue; // stale pooled socket: retry, fresh this time
            }
            // Nothing reached the client yet: answer, then close.
            Ok(Err(_)) => {
                log_fail(502, Some("no-response"));
                super::reply(client, RESP_502).await;
                return false;
            }
            Err(_) => {
                log_fail(504, Some("no-response")); // idle timeout on the response head
                super::reply(client, RESP_504).await;
                return false;
            }
        }
    };

    // B2: informational responses (1xx — e.g. `100 Continue`) are not
    // final. Forward each verbatim and re-read the real head on the same
    // socket; never pool in between (the response is still in flight).
    let mut hops = 0u8;
    loop {
        if !(100..200).contains(&rhead.status) {
            break;
        }
        hops += 1;
        if hops > 3 {
            return false;
        }
        if relay::write_flush(client, &resp_buf[..rhead.raw_len])
            .await
            .is_err()
        {
            return false;
        }
        // Bytes read past the 1xx head (often the final head, same
        // segment) start the next head: passed as prefix, since
        // read_response_head clears its buffer.
        let residual = resp_buf[rhead.raw_len..].to_vec();
        match read_response_head(&mut up, resp_buf, max_head, idle, &residual).await {
            Ok(h) => rhead = h,
            _ => return false, // backend died after a 1xx: drop safely
        }
    }

    // Body end: Content-Length, zero for HEAD/204/304 (B5: case-insensitive
    // and chunked must not override a declared zero body), chunked framing,
    // or unknown (then: relay until EOF, no pooling).
    let zero_body = rhead.no_body_status || head.method.slice(buf).eq_ignore_ascii_case(b"HEAD");
    let body_left = if zero_body {
        Some(0)
    } else {
        rhead.content_length
    };
    let chunked = rhead.chunked && !zero_body;
    let spare = (resp_buf.len() - rhead.raw_len) as u64;
    let forward_n = match body_left {
        // Chunked: early body bytes stay in `resp_buf`, the framing parser
        // must see them (it forwards them itself).
        _ if chunked => 0,
        Some(l) => l.min(spare) as usize,
        None => spare as usize,
    };
    // Forward what was already read (= the response head AND, if pipelined
    // into the same packets, early body bytes), capped at the body length
    // (anything beyond would belong to a backend-pipelined response — not
    // supported, dropped): the head itself is relayed verbatim (transparent
    // proxy).
    // Response body budget (config):
    // - declared Content-Length over the limit: answer 502 BEFORE
    //   forwarding the upstream head (clean, the client understands);
    // - unknown length (chunked / until-EOF): relay until the budget is
    //   consumed, then close the connection — a truncated stream is the
    //   only honest signal once the head has been forwarded.
    let over_declared = cfg.http.max_response_body_bytes > 0
        && body_left
            .map(|l| l > cfg.http.max_response_body_bytes)
            .unwrap_or(false);
    if over_declared {
        log_fail(502, Some("over-budget"));
        super::reply(client, RESP_502).await;
        return false;
    }

    let window_end = (rhead.raw_len + forward_n).min(resp_buf.len());
    if window_end > 0
        && relay::write_flush(client, &resp_buf[..window_end])
            .await
            .is_err()
    {
        return false;
    }
    let remaining: Option<u64> = body_left.map(|l| l - forward_n as u64);

    // Relay the rest of the body (zero-copy when the flow is bulk), or the
    // chunk framing transparently, or until EOF.
    // B1: no external absolute timeout — the engines carry their own
    // inactivity deadline (reset on every progress step).
    // Unknown-length bodies (chunked / until-EOF) are budget-capped: the
    // relay stops at the limit, then the connection is dropped (see
    // `budget_cut` below) — a truncated stream is the honest signal.
    let budget = cfg.http.max_response_body_bytes;
    let until_eof_capped = budget > 0 && body_left.is_none() && !rhead.chunked && !zero_body;
    let remaining_capped = if until_eof_capped {
        Some(budget.saturating_sub(forward_n as u64))
    } else {
        remaining
    };
    let (relaid, over_read) = if chunked {
        match relay::chunked(&mut up, client, resp_buf, rhead.raw_len, budget, idle).await {
            Ok(end) => (end.body, end.over_read),
            Err(e) => {
                tracing::debug!(upstream = %key, error = %e, "chunked relay aborted");
                log_fail(rhead.status, Some("truncated"));
                return false; // close: truncated stream is the honest signal
            }
        }
    } else {
        let n = relay::response(&mut up, client, remaining_capped, idle).await;
        // B4: bytes buffered beyond the body belong to a backend-pipelined
        // response — the stream is desynchronized, never pool it.
        (n, resp_buf.len() > window_end)
    };

    // Pool the upstream when the response allows clean reuse.
    let over_budget = budget > 0 && forward_n as u64 + relaid > budget;
    let complete = match body_left {
        Some(left) => forward_n as u64 + relaid == left,
        None => !over_budget, // chunked reached its terminal chunk within budget
    };
    if !complete || over_budget {
        if over_budget {
            tracing::warn!(
                upstream = %key, http = "response-over-budget",
                limit = budget, relayed = forward_n as u64 + relaid
            );
        } else {
            tracing::warn!(
                upstream = %key, http = "incomplete-upstream-response",
                left = ?body_left, got = relaid
            );
        }
        log_fail(
            rhead.status,
            Some(if over_budget {
                "over-budget"
            } else {
                "truncated"
            }),
        );
        return false; // close: truncated stream is the honest signal
    }
    if let Some(t0) = t0 {
        svc.log.emit(
            svc.log
                .line()
                .kv("client", peer)
                .kv("via", C::VIA)
                .kv("kind", "http")
                .kv_bytes("method", method_bytes)
                .kv("target", log_target())
                .kv("status", rhead.status)
                .kv("up", sent)
                .kv("down", forward_n as u64 + relaid)
                .kv("ms", t0.elapsed().as_millis()),
        );
    }
    if rhead.keep_alive && !over_read {
        pool.put(key, up);
    }
    // A body delimited by the upstream's EOF is delimited the same way for
    // the client: only the close tells it the response is over.
    let eof_delimited = body_left.is_none() && !chunked;
    !(head.close || head.version_minor == 0 || close_after || eof_delimited)
}

/// Writes the rewritten request head plus the request body to the upstream.
/// Returns false when the client connection must be dropped. Sets
/// `*body_rejected` when the configured request-body budget was exceeded
/// (the caller answers 413 instead of relaying).
/// Per-connection config + scratch for the upstream write path (one place
/// to pass instead of five loose arguments).
struct UpWriteCtx<'a> {
    idle: Duration,
    max_request_body: u64,
    /// Set when the request-body budget was exceeded (caller answers 413).
    body_rejected: bool,
    /// Set when the client's chunked framing was invalid (caller answers
    /// 400).
    bad_request: bool,
    /// Set when the client stopped sending its body (nobody to answer).
    client_gone: bool,
    /// Bytes followed the chunked body (a pipelined request, unsupported):
    /// close after the response.
    close_after: bool,
    /// Request body bytes forwarded (access log).
    sent: u64,
    rewrite_buf: &'a mut Vec<u8>,
}

async fn forward_request<C: ClientStream>(
    client: &mut C,
    up: &mut TcpStream,
    buf: &[u8],
    head: &RequestHead,
    origin: Span,
    ctx: &mut UpWriteCtx<'_>,
) -> bool {
    let content_length = head.content_length.unwrap_or(0);

    // Request body budget: reject on the declared size BEFORE anything is
    // forwarded (the rewritten head is dropped too). No Content-Length in
    // V1 means no streaming request body — so the declared size is the
    // only way a body can exceed the budget.
    if ctx.max_request_body > 0 && content_length > ctx.max_request_body {
        ctx.body_rejected = true;
        return false;
    }

    crate::http::rewrite_head_into(
        ctx.rewrite_buf,
        &buf[..head.raw_len],
        head.method.slice(buf),
        origin.slice(buf),
    );
    if head.chunked {
        return forward_chunked(client, up, buf, head, ctx).await;
    }
    let mut body = &buf[head.raw_len..];
    if body.len() as u64 > content_length {
        // Bytes beyond the body: a pipelined request (unsupported). This
        // one is forwarded and answered, then the connection closes; the
        // client retries the rest on a new one (RFC 9112 §9.3.2), as for
        // bytes after a chunked body.
        body = &body[..content_length as usize];
        ctx.close_after = true;
    }
    if body.is_empty() {
        // Fast path (GET/HEAD, the keep-alive hot path): one write, no
        // iovec bookkeeping.
        if timeout(ctx.idle, up.write_all(ctx.rewrite_buf))
            .await
            .is_err()
        {
            return false;
        }
    } else {
        // Head + pipelined body prefix in ONE syscall (writev) instead of
        // two sequential write_all calls. Rare partial writes advance the
        // cursor and retry.
        let total = ctx.rewrite_buf.len() + body.len();
        let mut written = 0usize;
        loop {
            let (first, second): (&[u8], &[u8]) = if written >= ctx.rewrite_buf.len() {
                (&body[written - ctx.rewrite_buf.len()..], &[])
            } else {
                (&ctx.rewrite_buf[written..], body)
            };
            let iovecs = [std::io::IoSlice::new(first), std::io::IoSlice::new(second)];
            match timeout(ctx.idle, up.write_vectored(&iovecs)).await {
                Ok(Ok(n)) if n > 0 => {
                    written += n;
                    if written >= total {
                        break;
                    }
                }
                Ok(Ok(_)) => return false, // zero: peer closed
                Ok(Err(_)) => return false,
                Err(_) => return false, // idle deadline
            }
        }
    }
    let remaining = content_length - body.len() as u64;
    if remaining > 0 {
        let mut limited = EndWatch {
            inner: (&mut *client).take(remaining),
            ended: false,
        };
        // Inactivity-bounded (idle resets on progress): slow uploads no
        // longer die at the absolute cap.
        let copied = crate::relay::copy_limited(&mut limited, up, None, ctx.idle).await;
        if copied < remaining && limited.ended {
            // The client closed mid-body: the origin would wait for the
            // rest until its own timeout. Abort both sides now. (A short
            // copy on the upstream side goes on: the origin may have
            // answered early.)
            ctx.client_gone = true;
            return false;
        }
    }
    ctx.sent = content_length;
    true
}

/// Reader wrapper recording whether the client side ended (EOF or error)
/// before its declared body was complete.
struct EndWatch<R> {
    inner: R,
    ended: bool,
}

impl<R: AsyncRead + Unpin> AsyncRead for EndWatch<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        match &r {
            std::task::Poll::Ready(Ok(()))
                if buf.filled().len() == before && buf.remaining() > 0 =>
            {
                self.ended = true
            }
            std::task::Poll::Ready(Err(_)) => self.ended = true,
            _ => {}
        }
        r
    }
}

/// Chunked request body: head first, then the body relayed verbatim
/// (framing included) by the chunked relay — the rewrite scratch is its
/// staging buffer (the head is out), seeded with the body bytes read along
/// with the head. The body budget is enforced while relaying.
async fn forward_chunked<C: ClientStream>(
    client: &mut C,
    up: &mut TcpStream,
    buf: &[u8],
    head: &RequestHead,
    ctx: &mut UpWriteCtx<'_>,
) -> bool {
    if !matches!(
        timeout(ctx.idle, up.write_all(ctx.rewrite_buf)).await,
        Ok(Ok(()))
    ) {
        return false;
    }
    ctx.rewrite_buf.clear();
    ctx.rewrite_buf.extend_from_slice(&buf[head.raw_len..]);
    match relay::chunked(
        client,
        up,
        ctx.rewrite_buf,
        0,
        ctx.max_request_body,
        ctx.idle,
    )
    .await
    {
        Ok(end) => {
            ctx.close_after = end.over_read;
            ctx.sent = end.body;
            true
        }
        Err(e) => {
            tracing::debug!(error = %e, "chunked request body aborted");
            match e.kind() {
                std::io::ErrorKind::FileTooLarge => ctx.body_rejected = true,
                std::io::ErrorKind::InvalidData => ctx.bad_request = true,
                _ => {}
            }
            false
        }
    }
}

/// Builds the upstream connection to `host:port`:
/// - numeric IP: connect directly (no blocking resolver),
/// - hostname: DNS cache, else resolve once and cache (all addresses kept
///   and tried in order — a host's first A/AAAA record is not guaranteed
///   to answer).
pub(super) async fn connect_upstream(
    host: &str,
    port: u16,
    connect: Duration,
    dns: &DnsCache,
    key: &str,
    dest: &DestPolicy,
) -> Result<TcpStream, UpstreamError> {
    if !dest.port_allowed(port) {
        return Err(UpstreamError::Forbidden("port"));
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        // Fast path: numeric IP target, skip the blocking DNS pool entirely.
        let addr = std::net::SocketAddr::from((ip, port));
        if let Some(reason) = dest.addr_verdict(addr) {
            return Err(UpstreamError::Forbidden(reason));
        }
        match timeout(connect, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => {
                widen_buffers(&s);
                Ok(s)
            }
            Ok(Err(e)) => Err(UpstreamError::Other(e)),
            Err(_) => Err(UpstreamError::Timeout),
        }
    } else {
        // Resolve once and cache: without this, every CONNECT to a hostname
        // pays a getaddrinfo in the 512-thread blocking pool, and bursts
        // of CONNECTs overflow it and blow the connect timeout.
        let addrs = match dns.get(key) {
            Some(a) => a,
            None => match timeout(connect, tokio::net::lookup_host((host, port))).await {
                Ok(Ok(iter)) => {
                    let v: Vec<std::net::SocketAddr> = iter.collect();
                    if v.is_empty() {
                        return Err(UpstreamError::Other(std::io::Error::new(
                            std::io::ErrorKind::AddrNotAvailable,
                            "no address for host",
                        )));
                    }
                    dns.put(key.to_string(), v.clone());
                    v
                }
                Ok(Err(e)) => return Err(UpstreamError::Other(e)),
                Err(_) => return Err(UpstreamError::Timeout),
            },
        };
        // Policy on the resolved addresses (a public name pointing at a
        // private address is refused too): disallowed ones are skipped.
        if !addrs.iter().any(|&a| dest.addr_allowed(a)) {
            let reason = addrs
                .first()
                .and_then(|&a| dest.addr_verdict(a))
                .unwrap_or("private");
            return Err(UpstreamError::Forbidden(reason));
        }
        match timeout(connect, connect_first(&addrs, dest)).await {
            Ok(Ok(s)) => {
                widen_buffers(&s);
                Ok(s)
            }
            Ok(Err(e)) => Err(UpstreamError::Other(e)),
            Err(_) => Err(UpstreamError::Timeout),
        }
    }
}

/// One MB of kernel socket buffer on the upstream socket too (the
/// response->proxy leg benefits as much as the proxy->client one).
fn widen_buffers(sock: &TcpStream) {
    crate::server::widen_buffers(sock);
}

/// Appends `v`'s decimal digits without allocating (the origin key is a
/// connection-owned scratch; `port.to_string()` cost one malloc per
/// request in the keep-alive hot path).
pub(super) fn push_u16(s: &mut String, v: u16) {
    let mut digits = [0u8; 5];
    let mut n = 0;
    let mut p = v;
    loop {
        digits[n] = b'0' + (p % 10) as u8;
        n += 1;
        p /= 10;
        if p == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        s.push(digits[i] as char);
    }
}

/// Try each address in order until one connects (sequential, same as
/// tokio's `TcpStream::connect((host, port))` behavior).
async fn connect_first(
    addrs: &[std::net::SocketAddr],
    dest: &DestPolicy,
) -> std::io::Result<TcpStream> {
    let mut last = None;
    for &addr in addrs.iter().filter(|&&a| dest.addr_allowed(a)) {
        match TcpStream::connect(addr).await {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no address to try")
    }))
}

/// Splits an idle timeout (outer `Err`, answered 504) from the other head
/// errors, the shape the former outer `timeout()` gave.
fn split_timeout(
    r: Result<ResponseHead, RespError>,
) -> Result<Result<ResponseHead, RespError>, ()> {
    match r {
        Err(RespError::Head(HeadError::Io(ref io)))
            if io.kind() == std::io::ErrorKind::TimedOut =>
        {
            Err(())
        }
        r => Ok(r),
    }
}
