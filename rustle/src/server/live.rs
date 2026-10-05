//! Reloadable configuration: the immutable snapshot shards run on, and the
//! reload that replaces it.

use std::sync::Arc;

use anyhow::Context;

use super::Shared;
use crate::auth::Authenticator;
use crate::config::Config;
use crate::dns::DnsCache;
use crate::policy::{AuthLimiter, ClientPolicy, DestPolicy};

/// The config-derived state `reload` replaces, as one immutable snapshot.
/// Connections hold the snapshot they were accepted under until they end.
pub(super) struct Live {
    pub(super) cfg: Arc<Config>,
    pub(super) auth: Arc<Authenticator>,
    pub(super) dns: Arc<DnsCache>,
    pub(super) dest: Arc<DestPolicy>,
    pub(super) clients: Arc<ClientPolicy>,
    pub(super) limiter: Arc<AuthLimiter>,
    /// HTTPS proxy listener's acceptor (None = plain only).
    pub(super) tls: Option<tokio_rustls::TlsAcceptor>,
}

impl Live {
    /// Builds a snapshot, validating everything a config can get wrong
    /// (policies, TLS material). The DNS cache and the ban list survive a
    /// reload when their settings did not change.
    pub(super) fn build(cfg: Arc<Config>, prev: Option<&Live>, ktls: bool) -> anyhow::Result<Live> {
        let keep = |same: fn(&Config, &Config) -> bool| prev.filter(|p| same(&p.cfg, &cfg));
        let dns =
            match keep(|a, b| a.dns.ttl == b.dns.ttl && a.dns.max_entries == b.dns.max_entries) {
                Some(p) => p.dns.clone(),
                None => Arc::new(DnsCache::new(cfg.dns.ttl, cfg.dns.max_entries)),
            };
        let limiter = match keep(|a, b| {
            a.auth.max_failures == b.auth.max_failures
                && a.auth.failure_window == b.auth.failure_window
                && a.auth.ban_duration == b.auth.ban_duration
        }) {
            Some(p) => p.limiter.clone(),
            None => Arc::new(AuthLimiter::new(
                cfg.auth.max_failures,
                cfg.auth.failure_window,
                cfg.auth.ban_duration,
            )),
        };
        let tls = if cfg.tls.listen.is_empty() {
            None
        } else {
            Some(crate::tls::acceptor(&cfg.tls, ktls)?)
        };
        Ok(Live {
            auth: Arc::new(Authenticator::new(&cfg.auth.api_keys)),
            dest: Arc::new(DestPolicy::from_config(&cfg)?),
            clients: Arc::new(ClientPolicy::from_config(&cfg)?),
            dns,
            limiter,
            tls,
            cfg,
        })
    }
}

/// Settings only a restart can apply (listeners, threads, pools, the log
/// writer). `reload` keeps the running value and reports them.
fn restart_required(old: &Config, new: &Config) -> Vec<&'static str> {
    let mut v = Vec::new();
    macro_rules! keys {
        ($($a:ident.$b:ident),* $(,)?) => {$(
            if format!("{:?}", old.$a.$b) != format!("{:?}", new.$a.$b) {
                v.push(concat!(stringify!($a), ".", stringify!($b)));
            }
        )*};
    }
    keys!(
        server.listen,
        server.workers,
        server.pin_cores,
        server.admin_listen,
        server.socket_buffer_bytes,
        tls.listen,
        tls.ktls,
        socks5.listen,
        http.upstream_keepalive,
        http.max_idle_per_origin,
        log.access,
        log.output,
        log.file,
        log.target,
    );
    v
}

/// `new` with the restart-only settings of `old`: what actually runs.
fn effective(old: &Config, mut new: Config) -> Config {
    new.server = old.server.clone();
    new.socks5 = old.socks5.clone();
    new.log = old.log.clone();
    new.tls.listen = old.tls.listen.clone();
    new.tls.ktls = old.tls.ktls;
    new.http.upstream_keepalive = old.http.upstream_keepalive;
    new.http.max_idle_per_origin = old.http.max_idle_per_origin;
    new
}

/// Outcome of a successful reload.
#[derive(Debug, Default)]
pub struct Reloaded {
    /// Changed settings that need a restart (still running with the old value).
    pub restart_required: Vec<&'static str>,
}

impl std::fmt::Display for Reloaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "reloaded")?;
        if !self.restart_required.is_empty() {
            write!(
                f,
                "; restart required for: {}",
                self.restart_required.join(", ")
            )?;
        }
        Ok(())
    }
}

impl Shared {
    /// Validates `new`, then publishes it: shards pick it up for the
    /// connections they accept from then on. On error nothing changes.
    fn reload(&self, new: Config) -> anyhow::Result<Reloaded> {
        let _one_at_a_time = self.reload_lock.lock().unwrap();
        let prev = self.live.borrow().clone();
        let restart_required = restart_required(&prev.cfg, &new);
        let cfg = Arc::new(effective(&prev.cfg, new));
        let live = Live::build(cfg, Some(&prev), self.ktls)?;
        self.live.send_replace(Arc::new(live));
        Ok(Reloaded { restart_required })
    }
}

pub(super) fn reload_from_file(shared: &Shared) -> anyhow::Result<Reloaded> {
    let path = shared
        .config_path
        .as_deref()
        .context("no config file to reload (started without one)")?;
    let cfg = Config::load(path).with_context(|| format!("loading config {}", path.display()))?;
    shared.reload(cfg)
}
