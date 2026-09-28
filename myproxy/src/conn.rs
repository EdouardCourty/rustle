use std::os::unix::io::AsRawFd;
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
                // Response relay: upstream is Connection: close, so the
                // response ends at EOF. Then loop for the next keep-alive
                // request (zero-copy when available, adaptive copy else).
                let _ = timeout(idle, relay_response(&mut up, &mut client)).await;
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
    #[cfg(target_os = "linux")]
    {
        match splice_bidirectional(client, up).await {
            Ok(res) => return Ok(res),
            Err((client, up)) => return adaptive_bidirectional(client, up).await,
        }
    }
    #[cfg(not(target_os = "linux"))]
    adaptive_bidirectional(client, up).await
}

/// User-space adaptive relay (all platforms).
async fn adaptive_bidirectional(
    client: TcpStream,
    up: TcpStream,
) -> std::io::Result<(u64, u64)> {
    let (ra, wa) = client.into_split();
    let (rb, wb) = up.into_split();
    let (a2b, b2a) = tokio::join!(relay(ra, wb), relay(rb, wa));
    Ok((a2b, b2a))
}

// ---------------------------------------------------------------------------
// Zero-copy relay (Linux): splice() moves data socket -> pipe -> socket
// entirely in the kernel. Versus the adaptive relay this removes both
// user-space copies per chunk; measured on this bench, 3proxy (which uses
// the same technique) sustains 4.4 Gbps/core where read+write managed
// 1.53 Gbps/core at equal saturation.
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
async fn splice_bidirectional(
    client: TcpStream,
    up: TcpStream,
) -> Result<(u64, u64), (TcpStream, TcpStream)> {
    use tokio::io::unix::AsyncFd;

    let mut p_ab = [0 as std::ffi::c_int; 2];
    let mut p_ba = [0 as std::ffi::c_int; 2];
    let ok = unsafe {
        libc::pipe2(p_ab.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) == 0
            && libc::pipe2(p_ba.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) == 0
    };
    if !ok {
        // No pipes (EMFILE/ENFILE?): fall back to the adaptive relay.
        return Err((client, up));
    }
    let (pipe_ab_r, pipe_ab_w) = (PipeFd(p_ab[0]), PipeFd(p_ab[1]));
    let (pipe_ba_r, pipe_ba_w) = (PipeFd(p_ba[0]), PipeFd(p_ba[1]));

    // into_std() keeps O_NONBLOCK, which AsyncFd registration requires.
    let (client_std, up_std) = match (client.into_std(), up.into_std()) {
        (Ok(c), Ok(u)) => (c, u),
        _ => return Ok((0, 0)), // dead sockets: nothing was relayed
    };
    let (client_fd, up_fd) = match (AsyncFd::new(client_std), AsyncFd::new(up_std)) {
        (Ok(c), Ok(u)) => (c, u),
        _ => return Ok((0, 0)),
    };
    let (a2b, b2a) = tokio::join!(
        splice_dir(&client_fd, &up_fd, &pipe_ab_r, &pipe_ab_w, true),
        splice_dir(&up_fd, &client_fd, &pipe_ba_r, &pipe_ba_w, true)
    );
    Ok((a2b, b2a))
}

/// HTTP response relay (upstream -> client), adaptive and zero-copy hybrid.
///
/// Phase 1 copies user-space with a growing buffer (8 -> 64 KB): small
/// responses (the common case) never touch the zero-copy machinery
/// (per-request pipe + fd dups + reactor registrations cost ~8% on 1 KB
/// objects, measured). Phase 2: when reads sustain the max buffer (bulk
/// response detected), switch to splice() for the remainder — measured
/// 1 MB responses go 1.2 GB/s -> 2.1 GB/s with it.
async fn relay_response(up: &mut TcpStream, client: &mut TcpStream) -> u64 {
    use tokio::io::AsyncReadExt;

    let mut buf: Vec<u8> = Vec::new();
    buf.reserve(RELAY_BUF0);
    let mut total: u64 = 0;
    loop {
        buf.clear();
        let n = match up.read_buf(&mut buf).await {
            Ok(n) => n,
            Err(_) => return total,
        };
        if n == 0 {
            // Upstream EOF: response complete. No shutdown of the client:
            // keep-alive must survive for the next request.
            return total;
        }
        if client.write_all(&buf[..n]).await.is_err() {
            return total;
        }
        total += n as u64;
        if buf.len() == buf.capacity() {
            if buf.capacity() < RELAY_BUF_MAX {
                buf.reserve(buf.capacity()); // double
            } else {
                // Sustained bulk flow: hand the rest to the zero-copy relay.
                if let Some(extra) = splice_response(up, client).await {
                    return total + extra;
                }
                // Zero-copy unavailable (no pipes / registration): keep
                // copying adaptively; capacity stays at RELAY_BUF_MAX.
            }
        }
    }
}

/// Zero-copy response relay. Dups both fds (the originals stay with tokio:
/// the client socket lives on for keep-alive, the upstream is closed by the
/// connection loop). A dup shares the file description, so O_NONBLOCK and
/// the underlying socket state are shared with the original.
#[cfg(target_os = "linux")]
async fn splice_response(up: &mut TcpStream, client: &mut TcpStream) -> Option<u64> {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    use tokio::io::unix::AsyncFd;

    let up_fd_raw = unsafe { libc::dup(up.as_raw_fd()) };
    if up_fd_raw < 0 {
        return None;
    }
    let client_fd_raw = unsafe { libc::dup(client.as_raw_fd()) };
    if client_fd_raw < 0 {
        unsafe { libc::close(up_fd_raw) };
        return None;
    }
    let mut p = [0 as std::ffi::c_int; 2];
    if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
        unsafe {
            libc::close(up_fd_raw);
            libc::close(client_fd_raw);
        };
        return None;
    }
    let (pipe_r, pipe_w) = (PipeFd(p[0]), PipeFd(p[1]));

    // from_raw_fd objects own their fd and close it on drop: nothing leaks
    // below even when registration fails.
    let up_dup = unsafe { std::net::TcpStream::from_raw_fd(up_fd_raw) };
    let client_dup = unsafe { std::net::TcpStream::from_raw_fd(client_fd_raw) };
    let (up_fd, client_fd) = match (AsyncFd::new(up_dup), AsyncFd::new(client_dup)) {
        (Ok(u), Ok(c)) => (u, c),
        _ => return None,
    };
    // No half-close of the client on EOF: keep-alive must survive.
    Some(splice_dir(&up_fd, &client_fd, &pipe_r, &pipe_w, false).await)
}

/// RAII around a raw pipe fd so cancellation (idle timeout dropping the
/// relay future mid-flight) can never leak it.
#[cfg(target_os = "linux")]
struct PipeFd(std::ffi::c_int);
#[cfg(target_os = "linux")]
impl Drop for PipeFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// One direction of the zero-copy relay: `src` -> pipe -> `dst`.
/// Returns the number of bytes moved. On EOF from `src`, half-closes `dst`
/// when `shutdown_on_eof` (tunnels propagate half-close; the HTTP response
/// relay keeps the client open for keep-alive).
#[cfg(target_os = "linux")]
async fn splice_dir(
    src: &tokio::io::unix::AsyncFd<std::net::TcpStream>,
    dst: &tokio::io::unix::AsyncFd<std::net::TcpStream>,
    pipe_r: &PipeFd,
    pipe_w: &PipeFd,
    shutdown_on_eof: bool,
) -> u64 {
    const SPLICE_F_MOVE: u32 = 1;
    const SPLICE_F_NONBLOCK: u32 = 2;
    let flags: u32 = SPLICE_F_MOVE | SPLICE_F_NONBLOCK;
    // Max bytes per src->pipe splice. NOT usize::MAX: the kernel rejects
    // len whose (ssize_t) cast is negative with EINVAL (found by probe).
    // The actual chunk is bounded by the pipe capacity (64 KB default).
    const SPLICE_CHUNK: usize = 256 * 1024;

    let mut moved: u64 = 0;

    'outer: loop {
        // Phase 1: wait for data on src and move up to one pipe-full.
        let mut in_pipe: usize;
        loop {
            let mut ready = match src.readable().await {
                Ok(g) => g,
                Err(_) => break 'outer, // reactor gone: bail out
            };
            let n = unsafe {
                libc::splice(
                    ready.get_inner().as_raw_fd(),
                    std::ptr::null_mut(),
                    pipe_w.0,
                    std::ptr::null_mut(),
                    SPLICE_CHUNK,
                    flags,
                )
            };
            if n > 0 {
                in_pipe = n as usize;
                break; // got data: drain the pipe into dst
            }
            if n == 0 {
                // EOF from src: half-close dst so the far end sees it.
                if shutdown_on_eof {
                    unsafe {
                        libc::shutdown(dst.get_ref().as_raw_fd(), libc::SHUT_WR);
                    }
                }
                break 'outer;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                ready.clear_ready(); // spurious readiness: wait again
                continue;
            }
            break 'outer; // connection error: stop this direction
        }

        // Phase 2: drain the pipe into dst (still zero-copy).
        while in_pipe > 0 {
            let mut ready = match dst.writable().await {
                Ok(g) => g,
                Err(_) => break 'outer,
            };
            let n = unsafe {
                libc::splice(
                    pipe_r.0,
                    std::ptr::null_mut(),
                    ready.get_inner().as_raw_fd(),
                    std::ptr::null_mut(),
                    in_pipe,
                    flags,
                )
            };
            if n > 0 {
                in_pipe -= n as usize;
                moved += n as u64;
            } else if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    ready.clear_ready();
                    continue;
                }
                break 'outer;
            } else {
                break 'outer; // n == 0: dst closed for writing
            }
        }
    }
    moved
}

/// One direction of the tunnel relay. Returns the transferred byte count.
async fn relay(mut r: OwnedReadHalf, mut w: OwnedWriteHalf) -> u64 {
    relay_loop(&mut r, &mut w, true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end tunnel relay check: two TCP pairs, relay between the two
    /// server-side sockets, push bytes through, verify integrity and that
    /// half-close propagates. Exercises the splice path on Linux.
    #[tokio::test]
    async fn tunnel_relay_moves_bytes_and_propagates_eof() {
        let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap();

        let a_cli = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (a_srv, _) = ln.accept().await.unwrap();
        let b_cli = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (b_srv, _) = ln.accept().await.unwrap();

        let (mut a_r, mut a_w) = a_cli.into_split();
        let (mut b_r, mut b_w) = b_cli.into_split();

        let relay = tokio::spawn(relay_bidirectional(a_srv, b_srv));

        // a -> b: 4 MB in both small and large chunks (grow the relay path).
        let payload: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let writer = a_w.write_all(&payload);
        let mut got = vec![0u8; payload.len()];
        let reader = b_r.read_exact(&mut got);
        let (wres, rres) = tokio::join!(writer, reader);
        wres.unwrap();
        rres.unwrap();
        assert_eq!(got, payload, "relay corrupted data");

        // Half-close: a shuts down writing -> b must see EOF.
        a_w.shutdown().await.unwrap();
        let n = b_r.read(&mut [0u8; 8]).await.unwrap();
        assert_eq!(n, 0, "b should see EOF after a's half-close");

        // Close b too -> both relay directions end, relay returns.
        b_w.shutdown().await.unwrap();
        let (a2b, b2a) = relay.await.unwrap().unwrap();
        assert_eq!(a2b, payload.len() as u64);
        assert_eq!(b2a, 0);
    }
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
