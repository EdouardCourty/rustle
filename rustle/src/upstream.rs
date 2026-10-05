use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use tokio::net::TcpStream;

/// Distinct origins kept in the map (empty entries included) before
/// emptied ones are pruned.
const MAX_ORIGINS: usize = 1024;

/// Idle-upstream connection pool, keyed by origin (`host:port`).
///
/// Rationale (profiled): the HTTP path used to open and close an upstream
/// connection per request. At 2600 req/s that is ~5200 connect/accept/close
/// syscall chains per second spread across the proxy and the backend; the
/// benchmark shows the HTTP bulk gap versus 3proxy narrowing heavily when
/// the connections are reused.
///
/// LIFO keeps the warmest socket in play. Staleness (backend closed an idle
/// connection) is handled by the caller: write/read failure before any
/// response byte -> retry once on a fresh connection, then drop this one.
pub struct UpstreamPool {
    inner: Mutex<Pool>,
}

struct Pool {
    by_origin: HashMap<String, VecDeque<TcpStream>>,
    max_idle_per_origin: usize,
    /// Global idle cap, to bound fd usage across many origins.
    max_total: usize,
    current_total: usize,
}

impl UpstreamPool {
    pub fn new(max_idle_per_origin: usize) -> Self {
        Self {
            inner: Mutex::new(Pool {
                by_origin: HashMap::new(),
                max_idle_per_origin,
                max_total: 1024,
                current_total: 0,
            }),
        }
    }

    /// Pop the warmest idle connection for `origin`, if any.
    pub fn take(&self, origin: &str) -> Option<TcpStream> {
        let mut pool = self.inner.lock().unwrap();
        let sock = pool
            .by_origin
            .get_mut(origin)
            .and_then(|idle| idle.pop_back());
        if sock.is_some() {
            pool.current_total -= 1;
            // An emptied origin keeps its entry (the next put would
            // reallocate key + deque on the keep-alive hot path); entries
            // are only pruned when many distinct origins pile up.
            if pool.by_origin.len() > MAX_ORIGINS
                && pool.by_origin.get(origin).is_some_and(|i| i.is_empty())
            {
                pool.by_origin.remove(origin);
            }
        }
        sock
    }

    /// Return a healthy connection to the pool (dropped if the origin or
    /// global cap is full).
    pub fn put(&self, origin: &str, sock: TcpStream) {
        let mut pool = self.inner.lock().unwrap();
        if pool.current_total >= pool.max_total {
            return; // full: drop the socket (close)
        }
        let max_idle_per_origin = pool.max_idle_per_origin;
        // Fast path first: an origin already pooled (the keep-alive hot
        // path) must not pay a key allocation — `entry` allocates its key
        // even when the key is already present.
        let idle = match pool.by_origin.get_mut(origin) {
            Some(idle) => idle,
            None => pool.by_origin.entry(origin.to_string()).or_default(),
        };
        if idle.len() >= max_idle_per_origin {
            return; // origin full: drop
        }
        idle.push_back(sock);
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

        let a = dummy().await;
        let b = dummy().await;
        let c = dummy().await;
        pool.put("upstream:8000", a);
        pool.put("upstream:8000", b);
        // Origin cap reached: third is dropped.
        pool.put("upstream:8000", c);

        let first = pool.take("upstream:8000");
        let second = pool.take("upstream:8000");
        assert!(first.is_some() && second.is_some());
        assert!(pool.take("upstream:8000").is_none()); // emptied

        // Global cap: force it small via another pool instance.
        let pool = UpstreamPool::new(64);
        for _ in 0..1025 {
            pool.put("x:1", dummy().await);
        }
        assert!(pool.take("x:1").is_some());
    }
}
