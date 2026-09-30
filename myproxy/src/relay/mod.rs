//! Transport engines: move bytes across two sockets, fast.
//!
//! Engines know nothing about HTTP, pools or the pipeline — they receive
//! byte-stream boundaries (`limit`) and teardown semantics only:
//!
//! - [`bidirectional`]: CONNECT tunnel relay (Linux: zero-copy splice with
//!   half-close propagation; portable fallback: adaptive user-space copy);
//! - [`response`]: HTTP response relay (adaptive copy that spills to
//!   splice on sustained bulk, bounded by the declared body length);
//! - [`chunked`]: transparent framing relay for chunked bodies;
//! - [`adaptive`]: the plain copy engine used by all of the above.

mod adaptive;
mod chunked;
mod splice;

pub(crate) use adaptive::{copy_limited, write_flush};
pub(crate) use chunked::chunk_size;
pub use chunked::chunked;
pub use splice::PipePair;

use std::time::Duration;
use tokio::time::Instant;

use crate::stream::ClientStream;

/// Inactivity deadline (B1): reset on every successful transfer step and
/// checked at every park. Replaces the external absolute `timeout(idle,
/// ...)` which killed active tunnels/downloads longer than `idle` wall
/// clock. Anti-leak on the public internet is then TCP keepalive's job
/// (SO_KEEPALIVE + TCP_KEEPIDLE), not a wall-clock cap.
pub(crate) struct IdleTimer {
    idle: Duration,
    deadline: Instant,
}

impl IdleTimer {
    pub fn new(idle: Duration) -> Self {
        Self {
            idle,
            deadline: Instant::now() + idle,
        }
    }

    /// Call after every successful fill/drain (progress = new deadline).
    pub fn reset(&mut self) {
        self.deadline = Instant::now() + self.idle;
    }

    /// Park on `fut` until it resolves OR the inactivity deadline passes.
    /// Returns `None` on inactivity timeout.
    pub async fn park<F>(&mut self, fut: F) -> Option<F::Output>
    where
        F: std::future::Future,
    {
        tokio::select! {
            out = fut => Some(out),
            _ = tokio::time::sleep_until(self.deadline) => None,
        }
    }
}

/// Initial per-direction read buffer. Small on purpose: an idle tunnel only
/// faults in this many pages, and most real tunnels (TLS handshakes, first
/// requests) fit inside it.
pub(crate) const RELAY_BUF0: usize = 8 * 1024;
/// Cap after adaptive growth. A read that fills the buffer completely
/// doubles it (8 -> 16 -> 32 -> 64 KB), so a bulk flow settles at
/// `RELAY_BUF_MAX`: one read+write pair per 64 KB instead of per 8 KB.
pub(crate) const RELAY_BUF_MAX: usize = 64 * 1024;

/// Bidirectional tunnel relay with a per-direction adaptive buffer.
///
/// Why not `tokio::io::copy_bidirectional`: its fixed 8 KB buffer costs
/// ~2 syscalls per 8 KB of bulk data; the benchmark showed a 10x
/// CPU-efficiency gap versus 3proxy (0.46 vs 4.4 Gbps/core). Here a
/// direction that consistently fills its buffer grows it up to
/// `RELAY_BUF_MAX`, cutting syscalls per byte by up to 8x, while sparse
/// flows keep the small buffer. On Linux the bulk case is additionally
/// handed to a zero-copy splice engine.
///
/// Semantics match `copy_bidirectional`: both directions run to EOF; the
/// returned byte counts are cumulative per direction; EOF half-closes the
/// opposite write side.
///
/// A TLS client cannot be spliced (the socket carries records): it is
/// relayed through user-space copies.
pub async fn bidirectional<C: ClientStream>(
    client: C,
    up: tokio::net::TcpStream,
    idle: Duration,
) -> std::io::Result<(u64, u64)> {
    let client = match client.into_tcp() {
        Ok(tcp) => tcp,
        Err(tls) => return Ok(adaptive::bidirectional_io(tls, up, idle).await),
    };
    #[cfg(target_os = "linux")]
    {
        match splice::bidirectional(client, up, idle).await {
            Ok(res) => Ok(res),
            Err((client, up)) => adaptive::bidirectional(client, up, idle).await,
        }
    }
    #[cfg(not(target_os = "linux"))]
    adaptive::bidirectional(client, up).await
}

/// HTTP response relay (upstream -> client): adaptive copy that spills to
/// the zero-copy engine on sustained bulk, bounded by `remaining` body
/// bytes when known (keep-alive upstream: the next response may already be
/// queued; we must not over-read). EOF does *not* half-close the client
/// (keep-alive must survive for the next request).
///
/// `pipes` is the connection-owned reusable pipe pair: `None` until the
/// first bulk response; on zero-copy failure mid-relay the pair is
/// dropped (state unknown → fresh pair next time).
pub async fn response<C: ClientStream>(
    up: &mut tokio::net::TcpStream,
    client: &mut C,
    remaining: Option<u64>,
    pipes: &mut Option<PipePair>,
    idle: Duration,
) -> u64 {
    adaptive::response(up, client, remaining, pipes, idle).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

        let (_a_r, mut a_w) = a_cli.into_split();
        let (mut b_r, mut b_w) = b_cli.into_split();

        let relay = tokio::spawn(bidirectional(
            a_srv,
            b_srv,
            std::time::Duration::from_secs(60),
        ));

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
