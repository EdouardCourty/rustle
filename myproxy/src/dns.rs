use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Process-wide DNS cache for CONNECT targets.
///
/// Without it, every CONNECT to a hostname pays a `getaddrinfo` in tokio's
/// blocking pool (512 threads max). Under a connection burst the queue
/// alone exceeded the 5 s connect timeout (measured: 504s on ~400/25 000
/// CONNECTs) — the benchmark harness bypasses DNS, real workloads don't.
struct Inner {
    entries: HashMap<String, (Vec<std::net::SocketAddr>, Instant)>,
    ttl: Duration,
    max_entries: usize,
}

pub struct DnsCache {
    inner: Mutex<Inner>,
}

impl DnsCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                ttl,
                max_entries,
            }),
        }
    }

    /// Cached addresses for `host:port` if present and fresh. A host may
    /// resolve to several addresses (e.g. localhost -> ::1, 127.0.0.1); the
    /// caller tries them in order, matching tokio's `ToSocketAddrs`
    /// semantics.
    pub fn get(&self, key: &str) -> Option<Vec<SocketAddr>> {
        if self.ttl_is_zero() {
            return None;
        }
        let mut guard = self.inner.lock().unwrap();
        match guard.entries.get(key) {
            Some((addrs, expiry)) if *expiry > Instant::now() => Some(addrs.clone()),
            Some(_) => {
                guard.entries.remove(key);
                None
            }
            None => None,
        }
    }

    fn ttl_is_zero(&self) -> bool {
        self.inner.lock().unwrap().ttl.is_zero()
    }

    pub fn put(&self, key: String, addrs: Vec<SocketAddr>) {
        let mut guard = self.inner.lock().unwrap();
        if guard.ttl.is_zero() {
            return;
        }
        if guard.entries.len() >= guard.max_entries {
            // Bounded and simple: a full cache is wiped. A proxy sees a
            // hot set of destinations; the refill costs a few resolutions.
            guard.entries.clear();
        }
        let expiry = Instant::now() + guard.ttl;
        guard.entries.insert(key, (addrs, expiry));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_miss_and_expiry() {
        let cache = DnsCache::new(Duration::from_millis(50), 16);
        let key = "example.com:443".to_string();
        assert!(cache.get(&key).is_none());

        let addrs = vec![
            "127.0.0.1:443".parse().unwrap(),
            "[::1]:443".parse().unwrap(),
        ];
        cache.put(key.clone(), addrs.clone());
        assert_eq!(cache.get(&key), Some(addrs));

        std::thread::sleep(Duration::from_millis(70));
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn zero_ttl_disables() {
        let cache = DnsCache::new(Duration::ZERO, 16);
        cache.put("a:443".into(), vec!["127.0.0.1:443".parse().unwrap()]);
        assert!(cache.get("a:443").is_none());
    }

    #[test]
    fn cap_wipes() {
        let cache = DnsCache::new(Duration::from_secs(60), 2);
        cache.put("a:1".into(), vec!["127.0.0.1:1".parse().unwrap()]);
        cache.put("b:2".into(), vec!["127.0.0.1:2".parse().unwrap()]);
        cache.put("c:3".into(), vec!["127.0.0.1:3".parse().unwrap()]);
        assert!(cache.get("a:1").is_none());
        assert!(cache.get("c:3").is_some());
    }
}
