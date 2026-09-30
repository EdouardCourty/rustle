//! Adaptive user-space copy engine: read/write pairs with a lazily
//! reserved, growing buffer. Bulk flows reach `RELAY_BUF_MAX` (fewer
//! syscalls per byte); idle tunnels touch zero relay pages.
//!
//! RAM contract: the buffer starts as pure *virtual* capacity (`reserve`,
//! no zero-fill) and pages are touched only by bytes actually received.
//! Capacity doubles (up to `RELAY_BUF_MAX`) whenever a read fills the
//! buffer completely, i.e. only a sustained bulk flow pays for big buffers.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use super::{IdleTimer, RELAY_BUF0, RELAY_BUF_MAX};

/// One direction of the plain tunnel fallback. Returns the transferred
/// byte count.
pub(super) async fn relay(
    mut r: OwnedReadHalf,
    mut w: OwnedWriteHalf,
    idle: std::time::Duration,
) -> u64 {
    let mut timer = IdleTimer::new(idle);
    copy_loop(&mut r, &mut w, true, &mut timer).await
}

/// User-space adaptive relay (all platforms): both tunnel directions.
pub(super) async fn bidirectional(
    client: TcpStream,
    up: TcpStream,
    idle: std::time::Duration,
) -> std::io::Result<(u64, u64)> {
    let (ra, wa) = client.into_split();
    let (rb, wb) = up.into_split();
    let (a2b, b2a) = tokio::join!(relay(ra, wb, idle), relay(rb, wa, idle));
    Ok((a2b, b2a))
}

/// HTTP response relay: upstream -> client, adaptive with spill-to-splice
/// on sustained bulk (`limit` bounds the relayed body). `pipes` is the
/// connection-owned reusable pair (`None` = create on first bulk).
pub(super) async fn response(
    up: &mut TcpStream,
    client: &mut TcpStream,
    remaining: Option<u64>,
    pipes: &mut Option<super::PipePair>,
    idle: std::time::Duration,
) -> u64 {
    if remaining == Some(0) {
        return 0; // whole body arrived with the head: no buffer, no timer
    }
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
        if client.write_all(&buf[..n]).await.is_err() {
            return total;
        }
        total += n as u64;
        timer.reset();
        if buf.len() == buf.capacity() {
            if buf.capacity() < RELAY_BUF_MAX {
                buf.reserve(buf.capacity()); // double
            } else if remaining.is_none() || remaining.unwrap() > total {
                // Sustained bulk flow: hand the rest to the zero-copy relay
                // with the connection's reusable pipe pair.
                let limit = remaining.map(|l| l - total);
                let result = match pipes {
                    Some(pair) => super::splice::response(up, client, limit, pair, idle).await,
                    None => match super::splice::PipePair::new() {
                        Some(pair) => {
                            let r = super::splice::response(up, client, limit, &pair, idle).await;
                            *pipes = Some(pair);
                            r
                        }
                        None => None, // kernel refused: adaptive continues
                    },
                };
                if let Some(extra) = result {
                    return total + extra;
                }
                // Zero-copy failed mid-relay: the pipe state is unknown,
                // drop the pair so the next response starts fresh.
                *pipes = None;
                // Zero-copy unavailable: keep copying adaptively.
            }
        }
    }
}

/// Copy-only body relay (used inside chunked framing, where the underlying
/// source is a bounded `Take` wrapper that cannot feed the splice path).
pub async fn copy_limited<R>(
    up: &mut R,
    client: &mut TcpStream,
    remaining: Option<u64>,
    idle: std::time::Duration,
) -> u64
where
    R: tokio::io::AsyncRead + Unpin,
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
        let write = client.write_all(&buf[..n]);
        match timer.park(write).await {
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
            _ => return written, // I/O error / inactivity: stop
        };
        if n == 0 {
            if shutdown_on_eof {
                // EOF: half-close the write side so the far end sees EOF
                // (matches copy_bidirectional teardown).
                let _ = w.shutdown().await;
            }
            return written;
        }
        match timer.park(w.write_all(&buf[..n])).await {
            Some(Ok(_)) => {}
            _ => return written,
        }
        written += n as u64;
        timer.reset();
        // A full buffer means the flow is (or may become) bulk: grow.
        if buf.len() == buf.capacity() && buf.capacity() < RELAY_BUF_MAX {
            buf.reserve(buf.capacity()); // doubles capacity
        }
    }
}
