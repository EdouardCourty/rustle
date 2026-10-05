use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Process-wide DNS cache for CONNECT targets.
///
/// Without it, every CONNECT to a hostname pays a `getaddrinfo` in tokio's
/// blocking pool (512 threads max) — under a connection burst the queue
/// alone blew the connect timeout. The benchmark harness bypasses DNS,
/// real workloads don't.
pub struct DnsCache {
    entries: Mutex<HashMap<String, (Vec<SocketAddr>, Instant)>>,
    /// Immutable: read without the lock (0 = cache off, no lock at all).
    ttl: Duration,
    max_entries: usize,
}

impl DnsCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
            max_entries,
        }
    }

    /// Cached addresses for `host:port` if present and fresh. A host may
    /// resolve to several addresses (e.g. localhost -> ::1, 127.0.0.1); the
    /// caller tries them in order, matching tokio's `ToSocketAddrs`
    /// semantics.
    pub fn get(&self, key: &str) -> Option<Vec<SocketAddr>> {
        if self.ttl.is_zero() {
            return None;
        }
        let mut entries = self.entries.lock().unwrap();
        match entries.get(key) {
            Some((addrs, expiry)) if *expiry > Instant::now() => Some(addrs.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    pub fn put(&self, key: String, addrs: Vec<SocketAddr>) {
        if self.ttl.is_zero() {
            return;
        }
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.max_entries {
            // Bounded and simple: a full cache is wiped. A proxy sees a
            // hot set of destinations; the refill costs a few resolutions.
            entries.clear();
        }
        entries.insert(key, (addrs, Instant::now() + self.ttl));
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
