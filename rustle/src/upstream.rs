use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;

/// Distinct origins kept in the map (empty entries included) before
/// emptied ones are pruned.
const MAX_ORIGINS: usize = 1024;

/// An idle upstream connection older than this is not reused: origins close
/// idle connections after a few seconds to a minute (Apache and Node default
/// to 5 s), and a stale socket costs a failed write, and a 502 for a request
/// that cannot be replayed.
const IDLE_TTL: Duration = Duration::from_secs(15);

/// The global idle cap is this many times the per-origin cap, so a handful of
/// busy origins can each fill their share while the descriptors held by idle
/// sockets stay bounded.
const ORIGINS_PER_CAP: usize = 16;

/// Idle-upstream connection pool, keyed by origin (`host:port`).
///
/// Rationale (profiled): opening and closing an upstream connection per
/// request costs ~2 connect/accept/close syscall chains per request (~5200
/// per second at 2600 req/s) spread across the proxy and the backend;
/// reusing the connections removes them.
///
/// LIFO keeps the warmest socket in play. A full origin evicts its *oldest*
/// idle socket rather than the one being returned, and a socket idle longer
/// than [`IDLE_TTL`] is closed instead of reused (every socket below it in
/// the LIFO is older still: they go with it). Staleness that survives this
/// (backend closed an idle connection early) is handled by the caller:
/// write/read failure before any response byte -> retry once on a fresh
/// connection, then drop this one.
pub struct UpstreamPool {
    inner: Mutex<Pool>,
}

struct Pool {
    by_origin: HashMap<String, VecDeque<(TcpStream, Instant)>>,
    max_idle_per_origin: usize,
    /// Global idle cap, to bound fd usage across many origins.
    max_total: usize,
    current_total: usize,
    ttl: Duration,
}

impl UpstreamPool {
    pub fn new(max_idle_per_origin: usize) -> Self {
        Self::with_ttl(max_idle_per_origin, IDLE_TTL)
    }

    fn with_ttl(max_idle_per_origin: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Pool {
                by_origin: HashMap::new(),
                max_idle_per_origin,
                max_total: max_idle_per_origin.saturating_mul(ORIGINS_PER_CAP),
                current_total: 0,
                ttl,
            }),
        }
    }

    /// Pop the warmest idle connection for `origin`, if any and still fresh.
    pub fn take(&self, origin: &str) -> Option<TcpStream> {
        let mut pool = self.inner.lock().unwrap();
        let ttl = pool.ttl;
        let idle = pool.by_origin.get_mut(origin)?;
        let (sock, since) = idle.pop_back()?;
        let mut dropped = 1;
        let fresh = since.elapsed() <= ttl;
        if !fresh {
            // The warmest socket is already too old: so is everything below it.
            dropped += idle.len();
            idle.clear();
        }
        pool.current_total -= dropped;
        // An emptied origin keeps its entry (the next put would reallocate
        // key + deque on the keep-alive hot path); entries are only pruned
        // when many distinct origins pile up.
        if pool.by_origin.len() > MAX_ORIGINS
            && pool.by_origin.get(origin).is_some_and(|i| i.is_empty())
        {
            pool.by_origin.remove(origin);
        }
        fresh.then_some(sock)
    }

    /// Return a healthy connection to the pool. When the origin (or the
    /// global cap) is full, the origin's oldest idle socket makes room.
    pub fn put(&self, origin: &str, sock: TcpStream) {
        let mut pool = self.inner.lock().unwrap();
        let (max_idle, global_full) = (
            pool.max_idle_per_origin,
            pool.current_total >= pool.max_total,
        );
        if max_idle == 0 {
            return;
        }
        // Fast path first: an origin already pooled (the keep-alive hot
        // path) must not pay a key allocation — `entry` allocates its key
        // even when the key is already present.
        let idle = match pool.by_origin.get_mut(origin) {
            Some(idle) => idle,
            None => pool.by_origin.entry(origin.to_string()).or_default(),
        };
        if idle.len() >= max_idle || global_full {
            if idle.pop_front().is_none() {
                return; // nothing of this origin to evict: drop the new one
            }
            idle.push_back((sock, Instant::now()));
            return; // one out, one in: the totals are unchanged
        }
        idle.push_back((sock, Instant::now()));
        pool.current_total += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn dummy() -> TcpStream {
        // A connected socket is required: pair up two loopback connections.
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _c = std::net::TcpStream::connect(ln.local_addr().unwrap()).unwrap();
        let (s, _) = ln.accept().unwrap();
        s.set_nonblocking(true).unwrap(); // tokio refuses blocking fds
        TcpStream::from_std(s).unwrap()
    }

    #[tokio::test]
    async fn lifo_and_caps() {
        let pool = UpstreamPool::new(2);
        assert!(pool.take("upstream:8000").is_none());

        let (a, b, c) = (dummy().await, dummy().await, dummy().await);
        let (la, lb, lc) = (
            a.local_addr().unwrap(),
            b.local_addr().unwrap(),
            c.local_addr().unwrap(),
        );
        pool.put("upstream:8000", a);
        pool.put("upstream:8000", b);
        // Origin cap reached: the oldest idle socket (a) makes room for c.
        pool.put("upstream:8000", c);

        // LIFO: the warmest first.
        assert_eq!(
            pool.take("upstream:8000").unwrap().local_addr().unwrap(),
            lc
        );
        assert_eq!(
            pool.take("upstream:8000").unwrap().local_addr().unwrap(),
            lb
        );
        assert!(pool.take("upstream:8000").is_none()); // emptied, a was evicted
        let _ = la;

        // Global cap = ORIGINS_PER_CAP x the per-origin cap, shared by origins.
        let pool = UpstreamPool::new(2);
        for i in 0..40 {
            pool.put(&format!("o{i}:1"), dummy().await);
        }
        assert_eq!(
            pool.inner.lock().unwrap().current_total,
            2 * ORIGINS_PER_CAP
        );
    }

    #[tokio::test]
    async fn disabled_pool_keeps_nothing() {
        let pool = UpstreamPool::new(0);
        pool.put("x:1", dummy().await);
        assert!(pool.take("x:1").is_none());
    }

    #[tokio::test]
    async fn stale_sockets_are_not_reused() {
        let pool = UpstreamPool::with_ttl(8, Duration::from_millis(50));
        for _ in 0..3 {
            pool.put("x:1", dummy().await);
        }
        assert!(pool.take("x:1").is_some()); // fresh
        tokio::time::sleep(Duration::from_millis(80)).await;
        // Too old: nothing is returned and the older ones are gone too.
        assert!(pool.take("x:1").is_none());
        assert_eq!(pool.inner.lock().unwrap().current_total, 0);
        pool.put("x:1", dummy().await);
        assert!(pool.take("x:1").is_some());
    }
}
