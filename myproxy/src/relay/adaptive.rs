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

use super::{RELAY_BUF0, RELAY_BUF_MAX};

/// One direction of the plain tunnel fallback. Returns the transferred
/// byte count.
pub(super) async fn relay(mut r: OwnedReadHalf, mut w: OwnedWriteHalf) -> u64 {
    copy_loop(&mut r, &mut w, true).await
}

/// User-space adaptive relay (all platforms): both tunnel directions.
pub(super) async fn bidirectional(client: TcpStream, up: TcpStream) -> std::io::Result<(u64, u64)> {
    let (ra, wa) = client.into_split();
    let (rb, wb) = up.into_split();
    let (a2b, b2a) = tokio::join!(relay(ra, wb), relay(rb, wa));
    Ok((a2b, b2a))
}

/// HTTP response relay: upstream -> client, adaptive with spill-to-splice
/// on sustained bulk (`limit` bounds the relayed body).
pub(super) async fn response(
    up: &mut TcpStream,
    client: &mut TcpStream,
    remaining: Option<u64>,
) -> u64 {
    let mut buf: Vec<u8> = Vec::new();
    buf.reserve(RELAY_BUF0);
    let mut total: u64 = 0;
    loop {
        if let Some(l) = remaining {
            if total == l {
                return total; // body fully consumed, over-read avoided
            }
        }
        buf.clear();
        // Bound the read to what remains (a take wrapper caps reads even
        // when more bytes are queued on the socket).
        let n = match remaining {
            Some(l) => {
                let mut lim = (&mut *up).take(l - total);
                lim.read_buf(&mut buf).await
            }
            None => up.read_buf(&mut buf).await,
        };
        let n = match n {
            Ok(n) => n,
            Err(_) => return total,
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
        if buf.len() == buf.capacity() {
            if buf.capacity() < RELAY_BUF_MAX {
                buf.reserve(buf.capacity()); // double
            } else if remaining.is_none() || remaining.unwrap() > total {
                // Sustained bulk flow: hand the rest to the zero-copy relay.
                if let Some(extra) =
                    super::splice::response(up, client, remaining.map(|l| l - total)).await
                {
                    return total + extra;
                }
                // Zero-copy unavailable: keep copying adaptively.
            }
        }
    }
}

/// Copy-only body relay (used inside chunked framing, where the underlying
/// source is a bounded `Take` wrapper that cannot feed the splice path).
pub(super) async fn copy_limited<R>(
    up: &mut R,
    client: &mut TcpStream,
    remaining: Option<u64>,
) -> u64
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    buf.reserve(RELAY_BUF0);
    let mut total: u64 = 0;
    loop {
        if let Some(l) = remaining {
            if total == l {
                return total;
            }
        }
        buf.clear();
        let n = match remaining {
            Some(l) => {
                let mut lim = (&mut *up).take(l - total);
                lim.read_buf(&mut buf).await
            }
            None => up.read_buf(&mut buf).await,
        };
        let n = match n {
            Ok(n) => n,
            Err(_) => return total,
        };
        if n == 0 {
            return total;
        }
        if client.write_all(&buf[..n]).await.is_err() {
            return total;
        }
        total += n as u64;
        if buf.len() == buf.capacity() && buf.capacity() < RELAY_BUF_MAX {
            buf.reserve(buf.capacity());
        }
    }
}

/// Core adaptive copy loop shared by the tunnel fallback.
async fn copy_loop<R, W>(r: &mut R, w: &mut W, shutdown_on_eof: bool) -> u64
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
