//! TCP acceptor: shards, connection accounting, backpressure and dispatch
//! to the proxy pipeline.
//!
//! Shared-nothing multi-core: `workers` shards, each an OS thread running
//! its own `current_thread` runtime, its own `SO_REUSEPORT` listener (the
//! kernel spreads incoming connections) and its own upstream pool. A
//! connection lives and dies on the shard that accepted it — no
//! cross-thread wakeups, no work stealing, no pool socket migrating between
//! reactors. Only cold or read-only state is shared: config, auth keys, DNS
//! cache, and the global connection counters.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::accesslog::{AccessLog, Refusal};
use crate::config::Config;
use crate::proxy;
use crate::upstream::UpstreamPool;

mod control;
mod cpu;
mod live;
mod socket;
#[cfg(test)]
mod tests;

use control::{admin_loop, control_loop};
use cpu::{allowed_cpus, pin_to_cpu};
pub use live::Reloaded;
use live::{reload_from_file, Live};
pub(crate) use socket::widen_buffers;
use socket::{bind, bind_control, bind_group, set_socket_buffers};

/// Blocking-pool budget (getaddrinfo) split across shards.
const BLOCKING_THREADS_TOTAL: usize = 512;

/// Process-wide state, shared by every shard.
struct Shared {
    /// The current snapshot; shards watch it and rebuild their `Services`.
    live: watch::Sender<Arc<Live>>,
    /// Serializes reloads.
    reload_lock: Mutex<()>,
    /// Config file `reload` re-reads (None: reload unavailable).
    config_path: Option<PathBuf>,
    log: Arc<AccessLog>,
    /// Hand TLS sessions to the kernel after the handshake.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ktls: bool,
    /// Sessions offloaded to the kernel (stats, tests).
    ktls_sessions: AtomicU64,
    /// Live client connections, all shards (enforces `max_connections`).
    active: Arc<AtomicUsize>,
    /// High-water mark of `active` since the last heap trim.
    peak: AtomicUsize,
    /// Connections accepted per shard (balance check, logged at shutdown);
    /// one cache line each, shards do not share a line they write.
    accepted: Vec<PerShard>,
}

/// A running server: one thread per shard.
pub struct Server {
    addr: SocketAddr,
    tls_addr: Option<SocketAddr>,
    socks_addr: Option<SocketAddr>,
    shared: Arc<Shared>,
    control_path: Option<PathBuf>,
    shutdown: watch::Sender<bool>,
    shards: Vec<JoinHandle<anyhow::Result<()>>>,
}

/// Number of shards for `workers` (0 = one per available CPU).
fn shard_count(workers: usize) -> usize {
    if workers > 0 {
        return workers;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// A shard's listeners.
struct ShardListeners {
    plain: std::net::TcpListener,
    tls: Option<std::net::TcpListener>,
    socks: Option<std::net::TcpListener>,
    admin: Option<std::net::TcpListener>,
    control: Option<std::os::unix::net::UnixListener>,
}

/// Binds every shard's listeners, then starts the shards. Binding happens
/// here, synchronously, so errors surface before any thread starts and an
/// ephemeral port (`:0`) resolved by the first listener is shared by all.
#[cfg_attr(not(test), allow(dead_code))]
pub fn start(cfg: Arc<Config>) -> anyhow::Result<Server> {
    start_with(cfg, None)
}

/// `start`, remembering the config file so `reload` can re-read it.
pub fn start_with(cfg: Arc<Config>, config_path: Option<PathBuf>) -> anyhow::Result<Server> {
    set_socket_buffers(cfg.server.socket_buffer_bytes);
    let n = shard_count(cfg.server.workers);
    let (addr, plain) = bind_group(&cfg.server.listen, n, "listen")?;
    // kTLS: wanted and supported by this kernel (probed once).
    let ktls = !cfg.tls.listen.is_empty() && cfg.tls.ktls && ktls_available();
    if !cfg.tls.listen.is_empty() && cfg.tls.ktls && !ktls {
        tracing::warn!("kTLS unavailable (Linux only; kernel module `tls` loaded?): userspace TLS");
    }
    // HTTPS proxy and SOCKS5 listeners: same sharding.
    let live = Live::build(cfg.clone(), None, ktls)?;
    let (tls_addr, mut tls_listeners) = if cfg.tls.listen.is_empty() {
        (None, Vec::new())
    } else {
        let (a, v) = bind_group(&cfg.tls.listen, n, "tls")?;
        (Some(a), v)
    };
    let (socks_addr, mut socks_listeners) = if cfg.socks5.listen.is_empty() {
        (None, Vec::new())
    } else {
        let (a, v) = bind_group(&cfg.socks5.listen, n, "socks5")?;
        (Some(a), v)
    };
    let mut admin = if cfg.server.admin_listen.is_empty() {
        None
    } else {
        let a: SocketAddr = cfg
            .server
            .admin_listen
            .parse()
            .with_context(|| format!("invalid admin address: {}", cfg.server.admin_listen))?;
        Some(bind(a, false).with_context(|| format!("binding admin {a}"))?)
    };
    let (mut control, control_path) = match cfg.server.control_socket_path() {
        None => (None, None),
        Some(path) => match bind_control(&path) {
            Ok(l) => (Some(l), Some(path)),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "control socket disabled");
                (None, None)
            }
        },
    };
    let admin_addr = admin.as_ref().and_then(|l| l.local_addr().ok());
    let cpus = if cfg.server.pin_cores {
        allowed_cpus()
    } else {
        Vec::new()
    };
    // One pinned shard per CPU: steer each connection to the shard on the
    // CPU that received its SYN (kernel processing and the proxy stay on
    // the same core). Shards sharing CPUs keep the default hash.
    #[cfg(target_os = "linux")]
    if n > 1 && n <= cpus.len() {
        for group in [&plain, &tls_listeners, &socks_listeners] {
            if let Some(first) = group.first() {
                if let Err(e) = steer_by_cpu(first, &cpus[..n]) {
                    tracing::warn!(error = %e, "SO_ATTACH_REUSEPORT_CBPF failed: hash balancing");
                }
            }
        }
    }

    let shared = Arc::new(Shared {
        live: watch::channel(Arc::new(live)).0,
        reload_lock: Mutex::new(()),
        config_path,
        log: Arc::new(AccessLog::start(
            cfg.log.access,
            cfg.log.output,
            &cfg.log.file,
            cfg.log.target,
        )?),
        ktls,
        ktls_sessions: AtomicU64::new(0),
        active: Arc::new(AtomicUsize::new(0)),
        peak: AtomicUsize::new(0),
        accepted: (0..n).map(|_| PerShard(AtomicU64::new(0))).collect(),
    });
    tracing::info!(
        %addr,
        tls = ?tls_addr,
        ktls,
        socks5 = ?socks_addr,
        admin = ?admin_addr,
        control = ?control_path,
        shards = n,
        pinned = !cpus.is_empty(),
        max_connections = cfg.limits.max_connections,
        block_private = cfg.filter.block_private,
        "rustle listening",
    );

    let (shutdown, rx) = watch::channel(false);
    let mut shards = Vec::with_capacity(n);
    tls_listeners.reverse(); // popped in shard order below
    socks_listeners.reverse();
    for (id, plain) in plain.into_iter().enumerate() {
        let shared = shared.clone();
        let rx = rx.clone();
        let listeners = ShardListeners {
            plain,
            tls: tls_listeners.pop(),
            socks: socks_listeners.pop(),
            admin: if id == 0 { admin.take() } else { None },
            control: if id == 0 { control.take() } else { None },
        };
        let cpu = (!cpus.is_empty()).then(|| cpus[id % cpus.len()]);
        let handle = std::thread::Builder::new()
            .name(format!("rustle-shard-{id}"))
            .spawn(move || {
                if let Some(cpu) = cpu {
                    pin_to_cpu(cpu);
                }
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    // Small tasks + a large accept fan-in: 128 KB/task stacks
                    // instead of glibc/tokio defaults (2 MB reserve, ~12-20 KB
                    // touched per tunnel just for handshakes).
                    .thread_stack_size(128 * 1024)
                    .max_blocking_threads((BLOCKING_THREADS_TOTAL / n).max(8))
                    .build()?;
                rt.block_on(run_shard(id, shared, listeners, rx))
            })?;
        shards.push(handle);
    }
    Ok(Server {
        addr,
        tls_addr,
        socks_addr,
        shared,
        control_path,
        shutdown,
        shards,
    })
}

impl Server {
    /// Proxy listen address (resolved port when configured with `:0`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// TLS sessions handed to the kernel so far.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn ktls_sessions(&self) -> u64 {
        self.shared.ktls_sessions.load(Relaxed)
    }

    /// SOCKS5 listen address, when enabled.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn socks_addr(&self) -> Option<SocketAddr> {
        self.socks_addr
    }

    /// HTTPS proxy listen address, when enabled.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn tls_addr(&self) -> Option<SocketAddr> {
        self.tls_addr
    }

    /// Re-reads the config file and applies what can change without a
    /// restart (keys, policies, limits, timeouts, TLS certificate…) to the
    /// connections accepted from now on. On error the running config stays.
    pub fn reload(&self) -> anyhow::Result<Reloaded> {
        reload_from_file(&self.shared)
    }

    /// Reopens the access log file (SIGHUP, after logrotate).
    pub fn reopen_logs(&self) {
        self.shared.log.reopen();
    }

    /// Stops accepting on every shard, lets in-flight connections drain
    /// (5 s cap), and joins the shard threads.
    pub fn shutdown_and_join(self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(true);
        if let Some(p) = &self.control_path {
            let _ = std::fs::remove_file(p);
        }
        let mut result = Ok(());
        for h in self.shards {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => result = Err(e),
                Err(_) => result = Err(anyhow::anyhow!("shard thread panicked")),
            }
        }
        let per_shard: Vec<u64> = self
            .shared
            .accepted
            .iter()
            .map(|a| a.0.load(Relaxed))
            .collect();
        tracing::info!(
            remaining = self.shared.active.load(Relaxed),
            accepted = ?per_shard,
            // > 0: the kernel pipe budget was hit, those relays copied.
            degraded_pipes = crate::relay::degraded_pipes(),
            "shutdown complete"
        );
        result
    }
}

/// Decrements the global connection count when the connection task ends
/// (normal return or panic). Holds the shard's `Services`: its refcount
/// is touched by this shard only, unlike the process-wide `Shared`.
struct ConnSlot(Arc<proxy::Services>);

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Relaxed);
    }
}

#[repr(align(64))]
struct PerShard(AtomicU64);

/// Which listener a connection came from.
#[derive(Clone, Copy)]
enum Kind {
    Plain,
    Tls,
    Socks,
}

/// One shard: accept loops, one task per connection, its own upstream pool.
/// Shard 0 also serves the admin listener and the heap-trim task.
async fn run_shard(
    id: usize,
    shared: Arc<Shared>,
    listeners: ShardListeners,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let plain = TcpListener::from_std(listeners.plain)?;
    let tls_listener = listeners.tls.map(TcpListener::from_std).transpose()?;
    let socks_listener = listeners.socks.map(TcpListener::from_std).transpose()?;
    if let Some(admin) = listeners.admin {
        tokio::spawn(admin_loop(TcpListener::from_std(admin)?));
    }
    if let Some(control) = listeners.control {
        tokio::spawn(control_loop(
            tokio::net::UnixListener::from_std(control)?,
            shared.clone(),
        ));
    }
    #[cfg(all(
        target_env = "gnu",
        not(any(feature = "jemalloc", feature = "mimalloc"))
    ))]
    if id == 0 {
        tokio::spawn(trim_after_bursts(shared.clone()));
    }

    // Access log: this shard's batch goes to the writer every tick.
    if shared.log.errors() {
        let log = shared.log.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(AccessLog::TICK);
            loop {
                tick.tick().await;
                log.flush_local();
            }
        });
    }

    let mut live_rx = shared.live.subscribe();
    let mut live = live_rx.borrow_and_update().clone();
    let mut max_connections = live.cfg.limits.max_connections;
    // The pool is the shard's own and outlives reloads (its settings are
    // restart-only).
    let pool = Arc::new(UpstreamPool::new(if live.cfg.http.upstream_keepalive {
        live.cfg.http.max_idle_per_origin
    } else {
        0
    }));
    let services = |live: &Live| {
        Arc::new(proxy::Services {
            cfg: live.cfg.clone(),
            auth: live.auth.clone(),
            dns: live.dns.clone(),
            dest: live.dest.clone(),
            limiter: live.limiter.clone(),
            log: shared.log.clone(),
            active: shared.active.clone(),
            pool: pool.clone(),
        })
    };
    let mut svc = services(&live);

    loop {
        let (accepted, kind) = tokio::select! {
            a = plain.accept() => (a, Kind::Plain),
            a = accept_on(&tls_listener) => (a, Kind::Tls),
            a = accept_on(&socks_listener) => (a, Kind::Socks),
            _ = shutdown.changed() => break,
        };
        // A reload lands here, at the next accept: one atomic load when
        // nothing changed, no extra wakeup.
        if live_rx.has_changed().unwrap_or(false) {
            live = live_rx.borrow_and_update().clone();
            max_connections = live.cfg.limits.max_connections;
            svc = services(&live);
        }
        let (sock, peer) = match accepted {
            Ok(pair) => pair,
            Err(e) => {
                // EMFILE/ENFILE/ENOBUFS persist until fds free up:
                // back off instead of spinning a core on accept().
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        let ip = peer.ip();
        // Client allowlist: outsiders are closed before any byte is read.
        if !live.clients.allows(ip) {
            tracing::debug!(%ip, "client not in allow_clients, closed");
            shared.log.refused(ip, Refusal::Allowlist);
            continue;
        }
        // Banned for repeated wrong keys: closed before any byte is read
        // (lock-free while nobody failed).
        if live.limiter.banned(ip) {
            shared.log.refused(ip, Refusal::Banned);
            continue;
        }

        let now = shared.active.fetch_add(1, Relaxed) + 1;
        if now > max_connections {
            shared.active.fetch_sub(1, Relaxed);
            tracing::warn!(?peer, "max_connections reached, rejecting");
            reject(sock);
            continue;
        }
        // Plain load first: the RMW (exclusive cache line) only when the
        // mark actually rises.
        if now > shared.peak.load(Relaxed) {
            shared.peak.fetch_max(now, Relaxed);
        }
        shared.accepted[id].0.fetch_add(1, Relaxed);

        // TCP_NODELAY and the 1 MB buffers are inherited from the listener
        // (set once in `bind`): no per-connection setsockopt.
        let slot = ConnSlot(svc.clone());
        let svc = svc.clone();
        match kind {
            Kind::Plain => {
                tokio::spawn(async move {
                    let _slot = slot;
                    proxy::handle_conn(sock, ip, svc).await;
                });
            }
            Kind::Socks => {
                tokio::spawn(async move {
                    let _slot = slot;
                    proxy::handle_socks5(sock, ip, svc).await;
                });
            }
            Kind::Tls => {
                let Some(acceptor) = live.tls.clone() else {
                    continue;
                };
                #[cfg(target_os = "linux")]
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    let hs = svc.cfg.tls.handshake_timeout;
                    #[cfg(target_os = "linux")]
                    if shared.ktls {
                        // Handshake in rustls, then the kernel takes over.
                        let accept = async {
                            let tls = acceptor.accept(ktls::CorkStream::new(sock)).await?;
                            let h2 = tls.get_ref().1.alpn_protocol() == Some(b"h2");
                            Ok::<_, std::io::Error>((crate::ktls_offload::offload(tls).await?, h2))
                        };
                        match tokio::time::timeout(hs, accept).await {
                            Ok(Ok((k, h2))) => {
                                shared.ktls_sessions.fetch_add(1, Relaxed);
                                if h2 {
                                    proxy::handle_h2(k, ip, svc).await
                                } else {
                                    proxy::handle_conn(k, ip, svc).await
                                }
                            }
                            Ok(Err(e)) => {
                                tracing::debug!(error = %e, "tls handshake / ktls offload failed")
                            }
                            Err(_) => tracing::debug!("tls handshake timed out"),
                        }
                        return;
                    }
                    match tokio::time::timeout(
                        hs,
                        acceptor.accept(crate::stream::ReadAhead::new(sock)),
                    )
                    .await
                    {
                        Ok(Ok(tls)) => {
                            if tls.get_ref().1.alpn_protocol() == Some(b"h2") {
                                proxy::handle_h2(tls, ip, svc).await
                            } else {
                                proxy::handle_conn(tls, ip, svc).await
                            }
                        }
                        Ok(Err(e)) => tracing::debug!(error = %e, "tls handshake failed"),
                        Err(_) => tracing::debug!("tls handshake timed out"),
                    }
                });
            }
        }
    }

    // Graceful drain: let in-flight connections finish (capped to 5 s —
    // beyond that a deploy is more important than a stuck client). The
    // counter is global: every shard waits for the whole process.
    drop((plain, tls_listener, socks_listener));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while shared.active.load(Relaxed) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shared.log.flush_local();
    Ok(())
}

/// Accepts on an optional listener (pending forever when absent).
async fn accept_on(l: &Option<TcpListener>) -> std::io::Result<(TcpStream, SocketAddr)> {
    match l {
        Some(l) => l.accept().await,
        None => std::future::pending().await,
    }
}

/// Hands freed heap back to the OS after a connection burst. On a
/// single-threaded runtime every allocation lives in one glibc arena,
/// whose top is only trimmed when contiguous: the free holes left by a
/// 25k-tunnel burst kept RSS at its peak (125 MB) once the tunnels were
/// gone. Checked every few seconds; trims (all arenas) only when the live
/// connection count fell to under half of the high-water mark since the
/// last trim.
#[cfg(all(
    target_env = "gnu",
    not(any(feature = "jemalloc", feature = "mimalloc"))
))]
async fn trim_after_bursts(shared: Arc<Shared>) {
    const MIN_PEAK: usize = 1024;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        let now = shared.active.load(Relaxed);
        let peak = shared.peak.load(Relaxed);
        if peak >= MIN_PEAK && now * 2 < peak {
            // SAFETY: plain libc call, no pointers involved.
            unsafe { libc::malloc_trim(0) };
            shared.peak.store(now, Relaxed);
        }
    }
}

/// kTLS exists on Linux only.
fn ktls_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::ktls_offload::available()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn reject(mut sock: TcpStream) {
    use tokio::io::AsyncWriteExt;
    tokio::spawn(async move {
        let _ = sock
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
    });
}
