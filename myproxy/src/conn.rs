use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::auth::Authenticator;
use crate::config::Config;
use crate::dns::DnsCache;
use crate::http::{classify, read_head, rewrite_head, HeadError, Target};

const RESP_407: &[u8] = b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"myproxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_400: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_431: &[u8] = b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_502: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_504: &[u8] = b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_200_CONNECT: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

enum UpstreamError {
    Timeout,
    Other(std::io::Error),
}

pub async fn handle_conn(
    mut client: TcpStream,
    cfg: Arc<Config>,
    auth: Arc<Authenticator>,
    dns: Arc<DnsCache>,
) {
    let idle = cfg.timeouts.idle;
    let connect = cfg.timeouts.connect;
    let max_head = cfg.limits.max_header_bytes;

    // Grow-on-demand request buffer: an idle keep-alive connection parked in
    // read_head holds the vector's *capacity*, whose untouched pages cost
    // nothing; capacity is reused across requests on the same connection.
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let head = match read_head(&mut client, &mut buf, max_head, idle).await {
            Ok(Some(h)) => h,
            Ok(None) => return,
            Err(HeadError::Io(e)) => {
                tracing::trace!(error = %e, "client read error");
                return;
            }
            Err(HeadError::TooLarge) => {
                let _ = client.write_all(RESP_431).await;
                return;
            }
            Err(HeadError::Invalid) => {
                let _ = client.write_all(RESP_400).await;
                return;
            }
        };

        if !auth.check(head.auth.as_deref()) {
            let _ = client.write_all(RESP_407).await;
            return;
        }

        let target = match classify(&head) {
            Ok(t) => t,
            Err(_) => {
                let _ = client.write_all(RESP_400).await;
                return;
            }
        };

        match target {
            Target::Connect { host, port } => {
                // Preserve any early data the client sent after the CONNECT
                // head (spec-violating but seen in the wild).
                let early = buf[head.raw_len..].to_vec();
                // Free the head-read buffer: the tunnel relay has its own
                // buffers, and holding this one would inflate RAM/connection.
                drop(std::mem::take(&mut buf));
                match connect_upstream(&host, port, connect, &dns).await {
                    Ok(mut up) => {
                        let _ = up.set_nodelay(true);
                        if client.write_all(RESP_200_CONNECT).await.is_err() {
                            return;
                        }
                        if !early.is_empty() {
                            let _ = timeout(idle, up.write_all(&early)).await;
                        }
                        // Tunnel: relay both directions until one side closes.
                        match timeout(idle, relay_bidirectional(client, up))
                            .await
                        {
                            Ok(Ok((c2u, u2c))) => {
                                tracing::trace!(c2u, u2c, "tunnel closed");
                            }
                            Ok(Err(e)) => tracing::trace!(error = %e, "tunnel error"),
                            Err(_) => tracing::trace!("tunnel idle timeout"),
                        }
                    }
                    Err(UpstreamError::Timeout) => {
                        let _ = client.write_all(RESP_504).await;
                    }
                    Err(UpstreamError::Other(e)) => {
                        tracing::debug!(error = %e, "upstream connect failed");
                        let _ = client.write_all(RESP_502).await;
                    }
                }
                return;
            }
            Target::Http { host, port, origin } => {
                let mut up = match connect_upstream(&host, port, connect, &dns).await {
                    Ok(s) => s,
                    Err(UpstreamError::Timeout) => {
                        let _ = client.write_all(RESP_504).await;
                        return;
                    }
                    Err(UpstreamError::Other(e)) => {
                        tracing::debug!(error = %e, "upstream connect failed");
                        let _ = client.write_all(RESP_502).await;
                        return;
                    }
                };
                let _ = up.set_nodelay(true);

                if !forward_request(&mut client, &mut up, &buf, &head, &origin, idle).await {
                    return;
                }
                // Response relay: upstream is Connection: close, so the response
                // ends at EOF. Then loop for the next keep-alive request.
                // Adaptive buffer, no shutdown afterwards (the client stays
                // open for keep-alive).
                let _ = timeout(idle, relay_up2down(&mut up, &mut client)).await;
                if head.close || head.version_minor == 0 {
                    return;
                }
            }
        }
    }
}

/// Writes the rewritten request head plus the request body to the upstream.
/// Returns false when the client connection must be dropped.
async fn forward_request(
    client: &mut TcpStream,
    up: &mut TcpStream,
    buf: &[u8],
    head: &crate::http::RequestHead,
    origin: &str,
    idle: Duration,
) -> bool {
    let rewritten = rewrite_head(&buf[..head.raw_len], &head.method, origin);
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
        if timeout(idle, tokio::io::copy(&mut limited, up)).await.is_err() {
            return false;
        }
    }
    true
}

async fn connect_upstream(
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
        // All resolved addresses are kept and tried in order (a host's
        // first A/AAAA record is not guaranteed to answer), matching
        // tokio's `ToSocketAddrs` connect semantics.
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

// ---------------------------------------------------------------------------
// Tunnel relay
// ---------------------------------------------------------------------------

/// Initial per-direction read buffer. Small on purpose: an idle tunnel only
/// faults in this many pages, and most real tunnels (TLS handshakes, first
/// requests) fit inside it.
const RELAY_BUF0: usize = 8 * 1024;
/// Cap after adaptive growth. A read that fills the buffer completely
/// doubles it (8 -> 16 -> 32 -> 64 KB), so a bulk flow settles at
/// ``RELAY_BUF_MAX``: one read+write pair per 64 KB instead of per 8 KB.
const RELAY_BUF_MAX: usize = 64 * 1024;

/// Bidirectional relay with a per-direction adaptive buffer.
///
/// Why not `tokio::io::copy_bidirectional`: its fixed 8 KB buffer costs
/// ~2 syscalls per 8 KB of bulk data; the benchmark showed a 10x
/// CPU-efficiency gap versus 3proxy (0.46 vs 4.4 Gbps/core). Here a
/// direction that consistently fills its buffer grows it up to
/// `RELAY_BUF_MAX`, cutting syscalls per byte by up to 8x, while sparse
/// flows keep the small buffer.
///
/// Semantics match `copy_bidirectional`: both directions run to EOF; the
/// returned byte counts are cumulative per direction; the first I/O error
/// is propagated and shuts *this* direction down (the other keeps writing
/// what it already buffered — close enough for tunnel teardown, where the
/// errored side aborts the whole task poll as both halves are dropped).
async fn relay_bidirectional(
    client: TcpStream,
    up: TcpStream,
) -> std::io::Result<(u64, u64)> {
    let (ra, wa) = client.into_split();
    let (rb, wb) = up.into_split();
    let (a2b, b2a) = tokio::join!(relay(ra, wb), relay(rb, wa));
    Ok((a2b, b2a))
}

/// One direction of the tunnel relay. Returns the transferred byte count.
async fn relay(mut r: OwnedReadHalf, mut w: OwnedWriteHalf) -> u64 {
    relay_loop(&mut r, &mut w, true).await
}

/// HTTP response relay: upstream -> client until EOF. No shutdown on EOF
/// (the client connection stays open for keep-alive).
async fn relay_up2down<R, W>(r: &mut R, w: &mut W) -> u64
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    relay_loop(r, w, false).await
}

/// Core adaptive copy loop shared by both relays.
///
/// RAM contract: the buffer starts as pure *virtual* capacity (`reserve`,
/// no zero-fill) and pages are touched only by bytes actually received —
/// an idle tunnel faults in zero relay pages. Capacity doubles (up to
/// `RELAY_BUF_MAX`) whenever a read fills the buffer completely, i.e. only
/// a sustained bulk flow pays for big buffers.
async fn relay_loop<R, W>(r: &mut R, w: &mut W, shutdown_on_eof: bool) -> u64
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    buf.reserve(RELAY_BUF0); // virtual only: untouched pages cost no RSS
    let mut written: u64 = 0;
    loop {
        buf.clear(); // logical len 0, keep the reserved capacity
        let n = match r.read_buf(&mut buf).await {
            Ok(n) => n,
            Err(_) => return written, // I/O error: stop this direction
        };
        if n == 0 {
            if shutdown_on_eof {
                // EOF: half-close the write side so the far end sees EOF
                // (matches copy_bidirectional teardown).
                let _ = w.shutdown().await;
            }
            return written;
        }
        if w.write_all(&buf[..n]).await.is_err() {
            return written;
        }
        written += n as u64;
        // A full buffer means the flow is (or may become) bulk: grow.
        if buf.len() == buf.capacity() && buf.capacity() < RELAY_BUF_MAX {
            buf.reserve(buf.capacity()); // doubles capacity
        }
    }
}
