//! The HTTP forward pipeline: upstream connection (pooled or fresh),
//! request forwarding, response head classification, body relay with exact
//! bounds, and pooling decisions. One function per logical request from
//! the FSM in [`super`].

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::config::Config;
use crate::dns::DnsCache;
use crate::http::{read_response_head, HeadError, RequestHead, RespError, Span, Target};
use crate::relay;
use crate::relay::PipePair;
use crate::upstream::UpstreamPool;

use super::{RESP_502, RESP_504};

pub(crate) enum UpstreamError {
    Timeout,
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
pub(super) async fn http(
    client: &mut TcpStream,
    cfg: &Config,
    buf: &mut Vec<u8>,
    head: &RequestHead,
    target: &Target,
    dns: &DnsCache,
    pool: &UpstreamPool,
    pipes: &mut Option<PipePair>,
    // Connection-owned scratches (M10.2: zero alloc in the hot path —
    // reused across keep-alive requests, dropped with the connection).
    resp_buf: &mut Vec<u8>,
    rewrite_buf: &mut Vec<u8>,
    origin_key: &mut String,
) -> bool {
    let idle = cfg.timeouts.idle;
    let connect = cfg.timeouts.connect;
    let max_head = cfg.limits.max_header_bytes;
    // M10.3: host/origin are spans into `buf`; the origin key is still
    // rebuilt per request into the connection-owned scratch (port digits
    // appended without allocating).
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

    // Upstream connection: pooled reuse when possible, fresh otherwise. A
    // pooled connection that died while idle is retried once on a fresh
    // one (safe: no response byte has reached the client at that point).
    let mut attempt = 0;
    let (mut up, _from_pool, mut rhead) = loop {
        attempt += 1;
        let (mut up, from_pool) = match pool.take(&key) {
            Some(s) => (s, true),
            None => match connect_upstream(host_str, port, connect, dns, key).await {
                Ok(s) => (s, false),
                Err(UpstreamError::Timeout) => {
                    let _ = client.write_all(RESP_504).await;
                    return false;
                }
                Err(UpstreamError::Other(e)) => {
                    tracing::debug!(error = %e, "upstream connect failed");
                    let _ = client.write_all(RESP_502).await;
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
            && head.content_length.unwrap_or(0) as usize == buf.len().saturating_sub(head.raw_len);

        let mut body_rejected = false;
        if !forward_request(
            client,
            &mut up,
            buf,
            head,
            origin_span,
            idle,
            cfg.http.max_request_body_bytes,
            &mut body_rejected,
            rewrite_buf,
        )
        .await
        {
            // Body over budget: answer 413 and drop (the client may still
            // be uploading — the close aborts it, like nginx does).
            if body_rejected {
                let _ = client.write_all(RESP_413).await;
            }
            return false;
        }
        match timeout(
            idle,
            read_response_head(&mut up, resp_buf, max_head, idle, &[]),
        )
        .await
        {
            Ok(Ok(h)) => break (up, from_pool, h),
            // B6: retry once on ANY I/O failure of a pooled socket — clean
            // EOF or RST — as long as the request is replayable (B7).
            Ok(Err(RespError::Head(HeadError::Io(_))))
                if from_pool && attempt == 1 && replayable =>
            {
                continue; // stale/rst pooled socket: retry, fresh this time
            }
            Ok(Err(RespError::Closed)) if from_pool && attempt == 1 => {
                continue; // stale pooled socket: retry, fresh this time
            }
            Ok(Err(_)) => return false,
            Err(_) => return false, // idle timeout on the response head
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
        if client.write_all(&resp_buf[..rhead.raw_len]).await.is_err() {
            return false;
        }
        let mut next: Vec<u8> = resp_buf[rhead.raw_len..].to_vec(); // residual = next head prefix
        let head_res = timeout(
            idle,
            read_response_head(&mut up, &mut next, max_head, idle, &[]),
        )
        .await;
        match head_res {
            Ok(Ok(h)) => {
                rhead = h;
                *resp_buf = next;
            }
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
    let spare = (resp_buf.len() - rhead.raw_len) as u64;
    let forward_n = match body_left {
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
        let _ = client.write_all(RESP_502).await;
        return false;
    }

    let window_end = (rhead.raw_len + forward_n).min(resp_buf.len());
    if window_end > 0 && client.write_all(&resp_buf[..window_end]).await.is_err() {
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
    let relaid: Option<u64> = if rhead.chunked && !zero_body {
        Some(
            relay::chunked(&mut up, client, idle)
                .await
                .unwrap_or(u64::MAX),
        )
    } else {
        Some(relay::response(&mut up, client, remaining_capped, pipes, idle).await)
    };
    let Some(relaid) = relaid else {
        return false;
    };

    // B4: bytes buffered beyond the body belong to a backend-pipelined
    // response — the stream is desynchronized, never pool it.
    let over_read = resp_buf.len() > window_end;

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
        return false; // close: truncated stream is the honest signal
    }
    if rhead.keep_alive && !over_read {
        pool.put(&key, up);
    }
    !(head.close || head.version_minor == 0)
}

/// Writes the rewritten request head plus the request body to the upstream.
/// Returns false when the client connection must be dropped. Sets
/// `*body_rejected` when the configured request-body budget was exceeded
/// (the caller answers 413 instead of relaying).
async fn forward_request(
    client: &mut TcpStream,
    up: &mut TcpStream,
    buf: &[u8],
    head: &RequestHead,
    origin: Span,
    idle: Duration,
    max_request_body: u64,
    body_rejected: &mut bool,
    rewrite_buf: &mut Vec<u8>,
) -> bool {
    let content_length = head.content_length.unwrap_or(0);

    // Request body budget: reject on the declared size BEFORE anything is
    // forwarded (the rewritten head is dropped too). No Content-Length in
    // V1 means no streaming request body — so the declared size is the
    // only way a body can exceed the budget.
    if max_request_body > 0 && content_length > max_request_body {
        *body_rejected = true;
        return false;
    }

    crate::http::rewrite_head_into(
        rewrite_buf,
        &buf[..head.raw_len],
        head.method.slice(buf),
        origin.slice(buf),
    );
    let body = &buf[head.raw_len..];
    if body.len() as u64 > content_length {
        // Pipelined bytes beyond the body are not supported in V1.
        return false;
    }
    if body.is_empty() {
        // Fast path (GET/HEAD, the keep-alive hot path): one write, no
        // iovec bookkeeping.
        if timeout(idle, up.write_all(rewrite_buf)).await.is_err() {
            return false;
        }
    } else {
        // Head + pipelined body prefix in ONE syscall (writev) instead of
        // two sequential write_all calls. Rare partial writes advance the
        // cursor and retry.
        let total = rewrite_buf.len() + body.len();
        let mut written = 0usize;
        loop {
            let (first, second): (&[u8], &[u8]) = if written >= rewrite_buf.len() {
                (&body[written - rewrite_buf.len()..], &[])
            } else {
                (&rewrite_buf[written..], body)
            };
            let iovecs = [std::io::IoSlice::new(first), std::io::IoSlice::new(second)];
            match timeout(idle, up.write_vectored(&iovecs)).await {
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
        let mut limited = (&mut *client).take(remaining);
        // Inactivity-bounded (idle resets on progress): slow uploads no
        // longer die at the absolute cap.
        let _ = crate::relay::copy_limited(&mut limited, up, None, idle).await;
    }
    true
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
) -> Result<TcpStream, UpstreamError> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        // Fast path: numeric IP target, skip the blocking DNS pool entirely.
        let addr = std::net::SocketAddr::from((ip, port));
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
        // pays a getaddrinfo in the 512-thread blocking pool (measured:
        // bursts of CONNECTs overflow it and blow the connect timeout).
        let addrs = match dns.get(&key) {
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
        match timeout(connect, connect_first(&addrs)).await {
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
async fn connect_first(addrs: &[std::net::SocketAddr]) -> std::io::Result<TcpStream> {
    let mut last = None;
    for &addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no address to try")
    }))
}

#[allow(dead_code)] // HeadError import used by variant matching below
fn _unused(_e: HeadError) {}
