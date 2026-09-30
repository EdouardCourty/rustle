//! Access policy: which destinations the proxy may reach, and which
//! clients are temporarily refused after repeated bad credentials.
//!
//! - [`DestPolicy`]: port allowlist (checked before DNS) and address
//!   filter (checked on every *resolved* address, right before connect —
//!   a hostname resolving to a private address is refused too, DNS
//!   rebinding included).
//! - [`ClientPolicy`]: client IP allowlist, checked at accept on every
//!   listener (empty = everyone) — for a personal proxy, typically the VPN
//!   range: the rest of the internet never gets a byte read.
//! - [`AuthLimiter`]: per-source-IP failure counter; after `max_failures`
//!   wrong keys inside `failure_window` the IP is banned for
//!   `ban_duration` and its connections are closed at accept. Only
//!   *wrong* credentials count — a request without credentials is the
//!   normal 407 challenge of browsers.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::config::Config;

/// An IP network (`addr/prefix`).
#[derive(Debug, Clone, Copy)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let (a, p) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = a.trim().parse().with_context(|| format!("invalid CIDR {s}"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) => p.trim().parse::<u8>().with_context(|| format!("invalid CIDR {s}"))?,
            None => max,
        };
        anyhow::ensure!(prefix <= max, "invalid CIDR prefix {s}");
        Ok(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, canonical(ip)) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let mask = u32::MAX.checked_shl(32 - self.prefix as u32).unwrap_or(0);
                u32::from(n) & mask == u32::from(a) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let mask = u128::MAX.checked_shl(128 - self.prefix as u32).unwrap_or(0);
                u128::from(n) & mask == u128::from(a) & mask
            }
            _ => false,
        }
    }
}

/// IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is the IPv4 address it wraps.
pub(crate) fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => ip,
        },
        v4 => v4,
    }
}

/// Addresses that are not the public internet: loopback, private,
/// link-local (cloud metadata 169.254.169.254), CGNAT, "this network",
/// multicast, reserved, broadcast; IPv6 loopback/unspecified, ULA,
/// link-local, multicast.
pub fn is_non_public(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(a) => {
            let o = a.octets();
            a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_multicast()
                || a.is_broadcast()
                || a.is_unspecified()
                || o[0] == 0 // 0.0.0.0/8
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT
                || o[0] >= 240 // 240.0.0.0/4 reserved
                || a == Ipv4Addr::new(192, 0, 0, 170) // NAT64 discovery
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
                || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

/// Where the proxy may connect to.
#[derive(Debug, Clone, Default)]
pub struct DestPolicy {
    block_private: bool,
    deny: Vec<Cidr>,
    allowed_ports: Vec<u16>,
}

impl DestPolicy {
    pub fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            block_private: cfg.filter.block_private,
            deny: cfg
                .filter
                .deny
                .iter()
                .map(|s| Cidr::parse(s))
                .collect::<anyhow::Result<_>>()?,
            allowed_ports: cfg.filter.allowed_ports.clone(),
        })
    }

    /// Port allowlist (empty = every port).
    #[inline]
    pub fn port_allowed(&self, port: u16) -> bool {
        self.allowed_ports.is_empty() || self.allowed_ports.contains(&port)
    }

    /// Resolved address check, right before connect.
    #[inline]
    pub fn addr_allowed(&self, addr: SocketAddr) -> bool {
        self.addr_verdict(addr).is_none()
    }

    /// Why `addr` is refused (`None` = allowed): "private" | "deny".
    #[inline]
    pub fn addr_verdict(&self, addr: SocketAddr) -> Option<&'static str> {
        let ip = addr.ip();
        if self.block_private && is_non_public(ip) {
            Some("private")
        } else if self.deny.iter().any(|c| c.contains(ip)) {
            Some("deny")
        } else {
            None
        }
    }
}

/// Which clients may connect at all.
#[derive(Debug, Clone, Default)]
pub struct ClientPolicy {
    allow: Vec<Cidr>,
}

impl ClientPolicy {
    pub fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            allow: cfg
                .auth
                .allow_clients
                .iter()
                .map(|s| Cidr::parse(s))
                .collect::<anyhow::Result<_>>()?,
        })
    }

    #[inline]
    pub fn allows(&self, ip: IpAddr) -> bool {
        self.allow.is_empty() || self.allow.iter().any(|c| c.contains(ip))
    }
}

struct Failures {
    count: u32,
    window_start: Instant,
    banned_until: Option<Instant>,
}

/// Per-IP bad-credential limiter (shared by every shard and listener).
pub struct AuthLimiter {
    max_failures: u32,
    window: Duration,
    ban: Duration,
    entries: Mutex<HashMap<IpAddr, Failures>>,
    /// IPs currently tracked: the accept-time check skips the lock
    /// entirely while nobody failed (the normal state).
    tracked: AtomicUsize,
}

/// Bound on tracked IPs (a scan from many sources must not grow the map
/// without limit; expired entries are swept when it is reached).
const MAX_TRACKED: usize = 65_536;

impl AuthLimiter {
    pub fn new(max_failures: u32, window: Duration, ban: Duration) -> Self {
        Self {
            max_failures,
            window,
            ban,
            entries: Mutex::new(HashMap::new()),
            tracked: AtomicUsize::new(0),
        }
    }

    /// True when `ip` is currently banned.
    #[inline]
    pub fn banned(&self, ip: IpAddr) -> bool {
        if self.tracked.load(Relaxed) == 0 {
            return false;
        }
        let now = Instant::now();
        let map = self.entries.lock().unwrap();
        matches!(map.get(&ip), Some(f) if f.banned_until.is_some_and(|t| t > now))
    }

    /// Records a wrong key from `ip`; returns true when it triggers a ban.
    pub fn fail(&self, ip: IpAddr) -> bool {
        if self.max_failures == 0 {
            return false;
        }
        let now = Instant::now();
        let mut map = self.entries.lock().unwrap();
        if map.len() >= MAX_TRACKED && !map.contains_key(&ip) {
            map.retain(|_, f| {
                f.banned_until.is_some_and(|t| t > now) || now - f.window_start < self.window
            });
            if map.len() >= MAX_TRACKED {
                return false; // still full of live entries: do not grow
            }
        }
        let f = map.entry(ip).or_insert(Failures {
            count: 0,
            window_start: now,
            banned_until: None,
        });
        if now - f.window_start >= self.window || f.banned_until.is_some_and(|t| t <= now) {
            f.count = 0;
            f.window_start = now;
            f.banned_until = None;
        }
        f.count += 1;
        let banned = f.count >= self.max_failures && f.banned_until.is_none();
        if banned {
            f.banned_until = Some(now + self.ban);
            tracing::warn!(%ip, failures = f.count, ban = ?self.ban, "auth failures: client banned");
        }
        self.tracked.store(map.len(), Relaxed);
        banned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn non_public_ranges() {
        for s in [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.1",
            "169.254.169.254", "100.64.0.1", "0.0.0.0", "0.1.2.3", "224.0.0.1",
            "240.0.0.1", "255.255.255.255", "::1", "::", "fc00::1", "fd12::1",
            "fe80::1", "ff02::1", "::ffff:127.0.0.1", "::ffff:10.0.0.1",
        ] {
            assert!(is_non_public(ip(s)), "{s}");
        }
        for s in ["1.1.1.1", "8.8.8.8", "172.32.0.1", "100.128.0.1", "2606:4700::1111", "::ffff:1.1.1.1"] {
            assert!(!is_non_public(ip(s)), "{s}");
        }
    }

    #[test]
    fn cidr_match() {
        let c = Cidr::parse("203.0.113.0/24").unwrap();
        assert!(c.contains(ip("203.0.113.7")));
        assert!(!c.contains(ip("203.0.114.7")));
        assert!(c.contains(ip("::ffff:203.0.113.9")));
        let h = Cidr::parse("2001:db8::/32").unwrap();
        assert!(h.contains(ip("2001:db8:1::1")));
        assert!(!h.contains(ip("2001:db9::1")));
        assert!(Cidr::parse("1.2.3.4").unwrap().contains(ip("1.2.3.4")));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains(ip("9.9.9.9")));
        assert!(Cidr::parse("1.2.3.4/33").is_err());
    }

    #[test]
    fn dest_policy() {
        let mut cfg = Config::default();
        cfg.filter.block_private = true;
        cfg.filter.deny = vec!["203.0.113.0/24".into()];
        cfg.filter.allowed_ports = vec![443];
        let p = DestPolicy::from_config(&cfg).unwrap();
        assert!(p.port_allowed(443) && !p.port_allowed(22));
        assert!(!p.addr_allowed("10.0.0.1:443".parse().unwrap()));
        assert!(!p.addr_allowed("203.0.113.5:443".parse().unwrap()));
        assert!(p.addr_allowed("1.1.1.1:443".parse().unwrap()));
    }

    #[test]
    fn client_allowlist() {
        let mut cfg = Config::default();
        assert!(ClientPolicy::from_config(&cfg).unwrap().allows(ip("203.0.113.1")));
        cfg.auth.allow_clients = vec!["100.64.0.0/10".into(), "::1".into()];
        let p = ClientPolicy::from_config(&cfg).unwrap();
        assert!(p.allows(ip("100.101.102.103")));
        assert!(p.allows(ip("::ffff:100.64.0.1"))); // dual-stack listener
        assert!(p.allows(ip("::1")));
        assert!(!p.allows(ip("203.0.113.1")));
        assert!(!p.allows(ip("127.0.0.1")));
        cfg.auth.allow_clients = vec!["nope".into()];
        assert!(ClientPolicy::from_config(&cfg).is_err());
    }

    #[test]
    fn limiter_bans_then_expires() {
        let l = AuthLimiter::new(3, Duration::from_secs(60), Duration::from_millis(50));
        let a = ip("198.51.100.1");
        assert!(!l.banned(a));
        assert!(!l.fail(a));
        assert!(!l.fail(a));
        assert!(l.fail(a));
        assert!(l.banned(a));
        assert!(!l.banned(ip("198.51.100.2")));
        std::thread::sleep(Duration::from_millis(60));
        assert!(!l.banned(a));
        // A fresh window after the ban: counting restarts.
        assert!(!l.fail(a));
        assert!(!l.banned(a));
    }

    #[test]
    fn limiter_disabled() {
        let l = AuthLimiter::new(0, Duration::from_secs(60), Duration::from_secs(60));
        for _ in 0..10 {
            l.fail(ip("198.51.100.1"));
        }
        assert!(!l.banned(ip("198.51.100.1")));
    }
}
