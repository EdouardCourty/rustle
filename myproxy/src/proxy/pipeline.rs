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
use crate::http::{read_response_head, HeadError, RequestHead, RespError};
use crate::relay;
use crate::upstream::UpstreamPool;

use super::{RESP_502, RESP_504};

pub(crate) enum UpstreamError {
    Timeout,
    Other(std::io::Error),
}

/// Serves one HTTP request end-to-end (upstream fetch + body relay + pool
/// accounting). Returns `false` when the client connection must be dropped
/// (client asked to close, broken state); `true` keeps the keep-alive loop
/// alive. All error paths already wrote the error response to the client.
#[allow(clippy::too_many_arguments)]
pub(super) async fn http(
    client: &mut TcpStream,
    host: &str,
    port: u16,
    origin: &str,
    cfg: &Config,
    buf: &mut Vec<u8>,
    head: &RequestHead,
    dns: &DnsCache,
    pool: &UpstreamPool,
) -> bool {
    let idle = cfg.timeouts.idle;
    let connect = cfg.timeouts.connect;
    let max_head = cfg.limits.max_header_bytes;
    let key = format!("{host}:{port}");

    // Upstream connection: pooled reuse when possible, fresh otherwise. A
    // pooled connection that died while idle is retried once on a fresh
    // one (safe: no response byte has reached the client at that point).
    let mut attempt = 0;
    let (mut up, _from_pool, rhead, rbuf) = loop {
        attempt += 1;
        let (mut up, from_pool) = match pool.take(&key) {
            Some(s) => (s, true),
            None => match connect_upstream(host, port, connect, dns).await {
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
        let _ = up.set_nodelay(true);

        if !forward_request(client, &mut up, buf, head, origin, idle).await {
            return false;
        }
        let mut rbuf: Vec<u8> = Vec::new();
        rbuf.reserve(1024);
        match timeout(idle, read_response_head(&mut up, &mut rbuf, max_head, idle)).await {
            Ok(Ok(h)) => break (up, from_pool, h, rbuf),
            Ok(Err(RespError::Closed)) if from_pool && attempt == 1 => {
                continue; // stale pooled socket: retry, fresh this time
            }
            Ok(Err(_)) => return false,
            Err(_) => return false, // idle timeout on the response head
        }
    };

    // Body end: Content-Length, zero for HEAD/204/304, chunked framing, or
    // unknown (then: relay until EOF, no pooling).
    let body_left = if rhead.no_body_status || head.method == "HEAD" {
        Some(0)
    } else {
        rhead.content_length
    };
    let spare = (rbuf.len() - rhead.raw_len) as u64;
    let forward_n = match body_left {
        Some(l) => l.min(spare) as usize,
        None => spare as usize,
    };
    // Forward what was already read (= the response head AND, if pipelined
    // into the same packets, early body bytes), capped at the body length
    // (anything beyond would belong to a backend-pipelined response — not
    // supported, dropped): the head itself is relayed verbatim (transparent
    // proxy).
    let window_end = (rhead.raw_len + forward_n).min(rbuf.len());
    if window_end > 0 && client.write_all(&rbuf[..window_end]).await.is_err() {
        return false;
    }
    let remaining: Option<u64> = body_left.map(|l| l - forward_n as u64);

    // Relay the rest of the body (zero-copy when the flow is bulk), or the
    // chunk framing transparently, or until EOF.
    let relaid = if rhead.chunked {
        match timeout(idle, relay::chunked(&mut up, client)).await {
            Ok(Ok(n)) => Some(n),
            _ => None, // error or timeout: stop here
        }
    } else {
        Some(
            timeout(idle, relay::response(&mut up, client, remaining))
                .await
                .unwrap_or(0),
        )
    };
    let Some(relaid) = relaid else {
        return false;
    };

    // Pool the upstream when the response allows clean reuse.
    let complete = match body_left {
        Some(left) => forward_n as u64 + relaid == left,
        None => true, // chunked counts only until its terminal chunk
    };
    if !complete {
        tracing::warn!(
            upstream = %key, http = "incomplete-upstream-response",
            left = ?body_left, got = relaid
        );
        return false;
    }
    if rhead.keep_alive {
        pool.put(&key, up);
    }
    !(head.close || head.version_minor == 0)
}

/// Writes the rewritten request head plus the request body to the upstream.
/// Returns false when the client connection must be dropped.
async fn forward_request(
    client: &mut TcpStream,
    up: &mut TcpStream,
    buf: &[u8],
    head: &RequestHead,
    origin: &str,
    idle: Duration,
) -> bool {
    let rewritten = crate::http::rewrite_head(&buf[..head.raw_len], &head.method, origin);
    if timeout(idle, up.write_all(&rewritten)).await.is_err() {
        return false;
    }

    let content_length = head.content_length.unwrap_or(0);
    let body = &buf[head.raw_len..];
    if body.len() as u64 > content_length {
        // Pipelined bytes beyond the body are not supported in V1.
        return false;
    }
    if !body.is_empty() {
        if timeout(idle, up.write_all(body)).await.is_err() {
            return false;
        }
    }
    let remaining = content_length - body.len() as u64;
    if remaining > 0 {
        let mut limited = (&mut *client).take(remaining);
        if timeout(idle, tokio::io::copy(&mut limited, up))
            .await
            .is_err()
        {
            return false;
        }
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
) -> Result<TcpStream, UpstreamError> {
    let key = format!("{host}:{port}");
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        // Fast path: numeric IP target, skip the blocking DNS pool entirely.
        let addr = std::net::SocketAddr::from((ip, port));
        match timeout(connect, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => Ok(s),
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
                    dns.put(key.clone(), v.clone());
                    v
                }
                Ok(Err(e)) => return Err(UpstreamError::Other(e)),
                Err(_) => return Err(UpstreamError::Timeout),
            },
        };
        match timeout(connect, connect_first(&addrs)).await {
            Ok(Ok(s)) => Ok(s),
            Ok(Err(e)) => Err(UpstreamError::Other(e)),
            Err(_) => Err(UpstreamError::Timeout),
        }
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
