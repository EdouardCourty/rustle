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
#[cfg(target_os = "linux")]
mod splice;
#[cfg(not(target_os = "linux"))]
#[path = "splice_stub.rs"]
mod splice;

pub(crate) use adaptive::{copy_limited, write_flush};
pub(crate) use chunked::chunk_size;
pub use chunked::chunked;
pub use splice::degraded_pipes;

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
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
    /// Both directions of a tunnel: it is idle only when neither moved data.
    shared: Option<Arc<Shared>>,
}

/// Latest progress deadline of a tunnel, as nanoseconds since `base`.
struct Shared {
    base: Instant,
    ns: AtomicU64,
}

impl IdleTimer {
    pub fn new(idle: Duration) -> Self {
        Self {
            idle,
            deadline: Instant::now() + idle,
            shared: None,
        }
    }

    /// Timers for the two directions of one tunnel. A direction that stays
    /// quiet while the other one moves data is not idle: the tunnel is idle
    /// only when neither direction made progress for `idle`.
    pub fn pair(idle: Duration) -> (Self, Self) {
        let base = Instant::now();
        let deadline = base + idle;
        let shared = Arc::new(Shared {
            base,
            ns: AtomicU64::new(idle.as_nanos() as u64),
        });
        let timer = |shared| Self {
            idle,
            deadline,
            shared: Some(shared),
        };
        (timer(shared.clone()), timer(shared))
    }

    /// Call after every successful fill/drain (progress = new deadline).
    pub fn reset(&mut self) {
        self.deadline = Instant::now() + self.idle;
        if let Some(s) = &self.shared {
            s.ns.fetch_max((self.deadline - s.base).as_nanos() as u64, Relaxed);
        }
    }

    /// The deadline in force: this direction's, or the other direction's if
    /// it made progress later.
    fn current(&self) -> Instant {
        match &self.shared {
            Some(s) => self
                .deadline
                .max(s.base + Duration::from_nanos(s.ns.load(Relaxed))),
            None => self.deadline,
        }
    }

    /// Park on `fut` until it resolves OR the inactivity deadline passes.
    /// Returns `None` on inactivity timeout.
    pub async fn park<F>(&mut self, fut: F) -> Option<F::Output>
    where
        F: std::future::Future,
    {
        let mut fut = std::pin::pin!(fut);
        loop {
            let deadline = self.current();
            // biased: poll the I/O first, no RNG draw per park.
            tokio::select! {
                biased;
                out = &mut fut => return Some(out),
                _ = tokio::time::sleep_until(deadline) => {}
            }
            // The other direction may have moved data while we waited.
            if self.current() <= deadline {
                return None;
            }
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
/// Cap for flows that can never splice (TLS client side: records, not
/// bytes): a bigger copy means fewer read/encrypt/write rounds per MB.
pub(crate) const RELAY_BUF_MAX_COPY: usize = 256 * 1024;

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
    match client.into_tcp() {
        // kTLS client (TLS session, plaintext socket): close_notify on EOF.
        Ok(tcp) => Ok(splice::tunnel(tcp, up, idle, C::TLS).await),
        Err(tls) => Ok(adaptive::bidirectional_io(tls, up, idle).await),
    }
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
    idle: Duration,
) -> u64 {
    adaptive::response(up, client, remaining, idle).await
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
    /// A tunnel whose client side stays quiet longer than the idle timeout
    /// while the other direction keeps moving data is not idle: what the
    /// client sends afterwards must still reach the upstream.
    #[tokio::test]
    async fn quiet_direction_survives_while_the_other_one_is_active() {
        use std::time::Duration;
        let idle = Duration::from_millis(300);
        let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap();
        let up_peer = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (up_srv, _) = ln.accept().await.unwrap();
        // The client side is a stand-in for a TLS stream (not a plain socket).
        let (client_peer, client_srv) = tokio::io::duplex(64 * 1024);

        let relay = tokio::spawn(adaptive::bidirectional_io(client_srv, up_srv, idle));
        let (mut up_r, mut up_w) = up_peer.into_split();
        let (mut cli_r, mut cli_w) = tokio::io::split(client_peer);

        // Upstream -> client: one byte every 50 ms for about a second.
        let feeder = tokio::spawn(async move {
            for _ in 0..20 {
                up_w.write_all(b"x").await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            up_w
        });
        let drain = tokio::spawn(async move {
            let mut b = [0u8; 64];
            while cli_r.read(&mut b).await.map(|n| n > 0).unwrap_or(false) {}
        });

        // The client says nothing for twice the idle timeout, then sends.
        tokio::time::sleep(idle * 2).await;
        cli_w.write_all(b"ping").await.unwrap();
        cli_w.flush().await.unwrap();

        let mut got = [0u8; 4];
        let r = tokio::time::timeout(Duration::from_millis(500), up_r.read_exact(&mut got)).await;
        assert!(
            matches!(r, Ok(Ok(_))) && &got == b"ping",
            "the client's data never reached the upstream: its direction was dropped as idle while the other was active"
        );
        let _ = feeder.await;
        drop(cli_w);
        drain.abort();
        relay.abort();
    }
    /// Same property on a plain socket pair (the zero-copy engine on Linux),
    /// and a tunnel where both directions are silent is still closed.
    #[tokio::test]
    async fn plain_tunnel_idle_only_when_both_directions_are_quiet() {
        use std::time::Duration;
        let idle = Duration::from_millis(300);
        let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap();
        let cli_peer = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (cli_srv, _) = ln.accept().await.unwrap();
        let up_peer = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (up_srv, _) = ln.accept().await.unwrap();

        let relay = tokio::spawn(bidirectional(cli_srv, up_srv, idle));
        let (mut cli_r, mut cli_w) = cli_peer.into_split();
        let (mut up_r, mut up_w) = up_peer.into_split();

        let feeder = tokio::spawn(async move {
            for _ in 0..20 {
                up_w.write_all(b"x").await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            up_w
        });
        let drain = tokio::spawn(async move {
            let mut b = [0u8; 64];
            while cli_r.read(&mut b).await.map(|n| n > 0).unwrap_or(false) {}
            cli_r
        });
        tokio::time::sleep(idle * 2).await;
        cli_w.write_all(b"ping").await.unwrap();
        let mut got = [0u8; 4];
        let r = tokio::time::timeout(Duration::from_millis(500), up_r.read_exact(&mut got)).await;
        assert!(
            matches!(r, Ok(Ok(_))) && &got == b"ping",
            "the client's data never reached the upstream"
        );

        // Nothing moves any more in either direction: the tunnel closes.
        let up_w = feeder.await.unwrap();
        let t0 = std::time::Instant::now();
        let ended = tokio::time::timeout(Duration::from_secs(3), relay).await;
        assert!(
            ended.is_ok(),
            "a tunnel silent in both directions stayed open"
        );
        assert!(t0.elapsed() < Duration::from_secs(2));
        drop((up_w, up_r, cli_w, drain));
    }
}
