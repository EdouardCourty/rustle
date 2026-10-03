//! Adaptive user-space copy engine: read/write pairs with a lazily
//! reserved, growing buffer. Bulk flows reach `RELAY_BUF_MAX` (fewer
//! syscalls per byte); idle tunnels touch zero relay pages.
//!
//! RAM contract: the buffer starts as pure *virtual* capacity (`reserve`,
//! no zero-fill) and pages are touched only by bytes actually received.
//! Capacity doubles (up to `RELAY_BUF_MAX`) whenever a read fills the
//! buffer completely, i.e. only a sustained bulk flow pays for big buffers.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::{IdleTimer, RELAY_BUF0, RELAY_BUF_MAX, RELAY_BUF_MAX_COPY};

/// Remaining declared body from which the response goes straight to splice.
const SPLICE_EARLY: u64 = 128 * 1024;
use crate::stream::ClientStream;

/// `write_all` + `flush`: TLS may keep ciphertext buffered after
/// `write_all` (see `stream`); the flush is a no-op on TCP.
pub(crate) async fn write_flush<W>(w: &mut W, buf: &[u8]) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    w.write_all(buf).await?;
    w.flush().await
}

/// Tunnel relay for a client that is not a plain socket (TLS): the client
/// is split through a lock (`tokio::io::split`), the upstream natively.
pub(super) async fn bidirectional_io<C>(
    client: C,
    up: TcpStream,
    idle: std::time::Duration,
) -> (u64, u64)
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut ra, mut wa) = tokio::io::split(client);
    let (mut rb, mut wb) = up.into_split();
    let mut ta = IdleTimer::new(idle);
    let mut tb = IdleTimer::new(idle);
    tokio::join!(
        copy_loop(&mut ra, &mut wb, true, &mut ta),
        copy_loop(&mut rb, &mut wa, true, &mut tb)
    )
}

/// HTTP response relay: upstream -> client, adaptive with spill-to-splice
/// on sustained bulk (`limit` bounds the relayed body).
pub(super) async fn response<C: ClientStream>(
    up: &mut TcpStream,
    client: &mut C,
    remaining: Option<u64>,
    idle: std::time::Duration,
) -> u64 {
    if remaining == Some(0) {
        return 0; // whole body arrived with the head: no buffer, no timer
    }
    // Known large body: zero-copy from the first byte (the copy ramp
    // 8 -> 64 KB moved ~120 KB of a 1 MB body through user space, with
    // four reallocations, before handing over).
    if remaining.is_some_and(|r| r >= SPLICE_EARLY) {
        if let Some(tcp) = client.tcp() {
            if let Some(pipe) = super::splice::PipePair::take() {
                return super::splice::response(up, tcp, remaining, &pipe, idle).await;
            }
        }
    }
    // Spliceable client: grow to 64 KB, then hand over to splice.
    // Otherwise (TLS) the copy is the whole relay: grow further.
    let max = if client.tcp().is_some() {
        RELAY_BUF_MAX
    } else {
        RELAY_BUF_MAX_COPY
    };
    let mut buf: Vec<u8> = Vec::with_capacity(RELAY_BUF0);
    let mut timer = IdleTimer::new(idle);
    let mut total: u64 = 0;
    loop {
        if let Some(l) = remaining {
            if total == l {
                return total; // body fully consumed, over-read avoided
            }
        }
        buf.clear();
        // Bound the read to what remains (a unified take wrapper caps
        // reads even when more bytes are queued on the socket) — a single
        // future type feeds the inactivity park.
        let want = remaining.unwrap_or(u64::MAX);
        let mut lim = (&mut *up).take(want.saturating_sub(total));
        let n = match timer.park(lim.read_buf(&mut buf)).await {
            Some(Ok(n)) => n,
            _ => return total, // I/O error / inactivity
        };
        if n == 0 {
            // Upstream EOF: fine when unlimited (Connection: close), an
            // error against a declared Content-Length (truncated body).
            return total;
        }
        match timer.park(write_flush(client, &buf[..n])).await {
            Some(Ok(_)) => {}
            _ => return total,
        }
        total += n as u64;
        timer.reset();
        if buf.len() == buf.capacity() {
            if buf.capacity() < max {
                buf.reserve(buf.capacity()); // double
            } else if remaining.is_none() || remaining.unwrap() > total {
                // Zero-copy needs the client socket itself (plain TCP) and
                // a usefully sized pipe (pooled per shard); otherwise keep
                // copying with the 64 KB buffer.
                let Some(client) = client.tcp() else {
                    continue;
                };
                let Some(pipe) = super::splice::PipePair::take() else {
                    continue;
                };
                let limit = remaining.map(|l| l - total);
                return total + super::splice::response(up, client, limit, &pipe, idle).await;
            }
        }
    }
}

/// Copy-only body relay (used inside chunked framing, where the underlying
/// source is a bounded `Take` wrapper that cannot feed the splice path).
pub async fn copy_limited<R, W>(
    up: &mut R,
    client: &mut W,
    remaining: Option<u64>,
    idle: std::time::Duration,
) -> u64
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(RELAY_BUF0);
    let mut timer = IdleTimer::new(idle);
    let mut total: u64 = 0;
    loop {
        if let Some(l) = remaining {
            if total == l {
                return total;
            }
        }
        buf.clear();
        // Unified take wrapper: a single future type feeds the park.
        let want = remaining.unwrap_or(u64::MAX);
        let mut lim = (&mut *up).take(want.saturating_sub(total));
        let n = match timer.park(lim.read_buf(&mut buf)).await {
            Some(Ok(n)) => n,
            _ => return total,
        };
        if n == 0 {
            return total;
        }
        match timer.park(write_flush(client, &buf[..n])).await {
            Some(Ok(_)) => {}
            _ => return total,
        }
        total += n as u64;
        timer.reset();
        if buf.len() == buf.capacity() && buf.capacity() < RELAY_BUF_MAX {
            buf.reserve(buf.capacity());
        }
    }
}

/// Core adaptive copy loop shared by the tunnel fallback.
async fn copy_loop<R, W>(r: &mut R, w: &mut W, shutdown_on_eof: bool, timer: &mut IdleTimer) -> u64
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(RELAY_BUF0); // virtual only: untouched pages cost no RSS
    let mut written: u64 = 0;
    loop {
        buf.clear(); // logical len 0, keep the reserved capacity
        let n = match timer.park(r.read_buf(&mut buf)).await {
            Some(Ok(n)) => n,
            // A read error ends this direction like EOF does: a TLS client
            // closing TCP without close_notify (common) surfaces as
            // UnexpectedEof, and must not leave the far end open.
            Some(Err(_)) => 0,
            None => return written, // inactivity: stop
        };
        if n == 0 {
            if shutdown_on_eof {
                // EOF: half-close the write side so the far end sees EOF
                // (matches copy_bidirectional teardown).
                let _ = w.shutdown().await;
            }
            return written;
        }
        match timer.park(write_flush(w, &buf[..n])).await {
            Some(Ok(_)) => {}
            _ => return written,
        }
        written += n as u64;
        timer.reset();
        // A full buffer means the flow is (or may become) bulk: grow.
        if buf.len() == buf.capacity() && buf.capacity() < RELAY_BUF_MAX_COPY {
            buf.reserve(buf.capacity()); // doubles capacity
        }
    }
}
