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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::accesslog::{AccessLog, Refusal};
use crate::auth::Authenticator;
use crate::config::Config;
use crate::dns::DnsCache;
use crate::policy::{AuthLimiter, ClientPolicy, DestPolicy};
use crate::proxy;
use crate::upstream::UpstreamPool;

/// Kernel socket buffer size (`server.socket_buffer_bytes`, 0 = autotuning).
/// One MB by default: the first in-flight burst before TCP autotuning
/// catches up was throttled by the 16 KB default (10 MB responses were
/// -11 % vs 3proxy). Applied best-effort on both sides of every proxied
/// socket. Kernel memory is allocated on demand (skb) — an idle connection
/// pays nothing. Process-wide: set once by `start`, read per upstream connect.
static SOCK_BUF: AtomicUsize = AtomicUsize::new(1024 * 1024);

/// Blocking-pool budget (getaddrinfo) split across shards.
const BLOCKING_THREADS_TOTAL: usize = 512;

/// Records the configured buffer size; warns when the kernel caps it (the
/// fixed size then both shrinks and disables autotuning: on a stock
/// rmem_max of 208 KB it bounds a 100 ms path to ~33 Mbit/s).
fn set_socket_buffers(bytes: usize) {
    SOCK_BUF.store(bytes, Relaxed);
    if bytes == 0 {
        return;
    }
    for sysctl in ["rmem_max", "wmem_max"] {
        let max = std::fs::read_to_string(format!("/proc/sys/net/core/{sysctl}"))
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok());
        if let Some(max) = max.filter(|&m| m < bytes) {
            tracing::warn!(
                socket_buffer_bytes = bytes,
                %sysctl,
                max,
                "socket buffer capped by the kernel and autotuning disabled: \
                 raise net.core.{sysctl} or set socket_buffer_bytes = 0"
            );
        }
    }
}

/// Widen SO_SNDBUF/SO_RCVBUF on a socket (best effort, silent).
pub(crate) fn widen_buffers(sock: &TcpStream) {
    use std::os::unix::io::AsRawFd;
    let Ok(buf) = libc::c_int::try_from(SOCK_BUF.load(Relaxed)) else {
        return;
    };
    if buf == 0 {
        return; // autotuning
    }
    let fd = sock.as_raw_fd();
    // SAFETY: valid connected fd borrowed from `sock`; the option value is
    // a c_int that outlives both calls.
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &buf as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &buf as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// Process-wide state, shared by every shard.
struct Shared {
    cfg: Arc<Config>,
    auth: Arc<Authenticator>,
    dns: Arc<DnsCache>,
    dest: Arc<DestPolicy>,
    clients: ClientPolicy,
    limiter: Arc<AuthLimiter>,
    log: Arc<AccessLog>,
    /// HTTPS proxy listener's acceptor (None = plain only).
    tls: Option<tokio_rustls::TlsAcceptor>,
    /// Hand TLS sessions to the kernel after the handshake.
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

/// Binds every shard's listener, then starts the shards. Binding happens
/// here, synchronously, so errors surface before any thread starts and an
/// ephemeral port (`:0`) resolved by the first listener is shared by all.
/// Binds one listener per shard on `listen` (SO_REUSEPORT when sharded;
/// a `:0` port resolved by the first is shared by the others).
fn bind_group(
    listen: &str,
    n: usize,
    what: &str,
) -> anyhow::Result<(SocketAddr, Vec<std::net::TcpListener>)> {
    let a: SocketAddr = listen
        .parse()
        .with_context(|| format!("invalid {what} address: {listen}"))?;
    let first = bind(a, n > 1).with_context(|| format!("binding {what} {a}"))?;
    let addr = first.local_addr()?;
    let mut v = vec![first];
    for _ in 1..n {
        v.push(bind(addr, true).with_context(|| format!("binding {what} {addr}"))?);
    }
    Ok((addr, v))
}

/// A shard's listeners.
struct ShardListeners {
    plain: std::net::TcpListener,
    tls: Option<std::net::TcpListener>,
    socks: Option<std::net::TcpListener>,
    admin: Option<std::net::TcpListener>,
}

/// Binds every shard's listeners, then starts the shards. Binding happens
/// here, synchronously, so errors surface before any thread starts and an
/// ephemeral port (`:0`) resolved by the first listener is shared by all.
pub fn start(cfg: Arc<Config>) -> anyhow::Result<Server> {
    set_socket_buffers(cfg.server.socket_buffer_bytes);
    let n = shard_count(cfg.server.workers);
    let (addr, plain) = bind_group(&cfg.server.listen, n, "listen")?;
    // kTLS: wanted and supported by this kernel (probed once).
    let ktls = !cfg.tls.listen.is_empty() && cfg.tls.ktls && crate::ktls_offload::available();
    if !cfg.tls.listen.is_empty() && cfg.tls.ktls && !ktls {
        tracing::warn!("kTLS unavailable (kernel module `tls` not loaded?): userspace TLS");
    }
    // HTTPS proxy and SOCKS5 listeners: same sharding.
    let (tls, tls_addr, mut tls_listeners) = if cfg.tls.listen.is_empty() {
        (None, None, Vec::new())
    } else {
        let acceptor = crate::tls::acceptor(&cfg.tls, ktls)?;
        let (a, v) = bind_group(&cfg.tls.listen, n, "tls")?;
        (Some(acceptor), Some(a), v)
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
    let cpus = if cfg.server.pin_cores {
        allowed_cpus()
    } else {
        Vec::new()
    };
    // One pinned shard per CPU: steer each connection to the shard on the
    // CPU that received its SYN (kernel processing and the proxy stay on
    // the same core). Shards sharing CPUs keep the default hash.
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
        auth: Arc::new(Authenticator::new(&cfg.auth.api_keys)),
        dns: Arc::new(DnsCache::new(cfg.dns.ttl, cfg.dns.max_entries)),
        dest: Arc::new(DestPolicy::from_config(&cfg)?),
        clients: ClientPolicy::from_config(&cfg)?,
        log: Arc::new(AccessLog::start(
            cfg.log.access,
            cfg.log.output,
            &cfg.log.file,
            cfg.log.target,
        )?),
        limiter: Arc::new(AuthLimiter::new(
            cfg.auth.max_failures,
            cfg.auth.failure_window,
            cfg.auth.ban_duration,
        )),
        tls,
        ktls,
        ktls_sessions: AtomicU64::new(0),
        active: Arc::new(AtomicUsize::new(0)),
        peak: AtomicUsize::new(0),
        accepted: (0..n).map(|_| PerShard(AtomicU64::new(0))).collect(),
        cfg,
    });
    tracing::info!(
        %addr,
        tls = ?tls_addr,
        ktls,
        socks5 = ?socks_addr,
        shards = n,
        pinned = !cpus.is_empty(),
        max_connections = shared.cfg.limits.max_connections,
        block_private = shared.cfg.filter.block_private,
        "myproxy listening",
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
        };
        let cpu = (!cpus.is_empty()).then(|| cpus[id % cpus.len()]);
        let handle = std::thread::Builder::new()
            .name(format!("myproxy-shard-{id}"))
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

    /// Reopens the access log file (SIGHUP, after logrotate).
    pub fn reopen_logs(&self) {
        self.shared.log.reopen();
    }

    /// Stops accepting on every shard, lets in-flight connections drain
    /// (5 s cap), and joins the shard threads.
    pub fn shutdown_and_join(self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(true);
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

/// One shard: accept loop, one task per connection, its own upstream pool.
/// Shard 0 also serves the admin listener and the heap-trim task.
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

    let cfg = shared.cfg.clone();
    let max_connections = cfg.limits.max_connections;
    let svc = Arc::new(proxy::Services {
        cfg: cfg.clone(),
        auth: shared.auth.clone(),
        dns: shared.dns.clone(),
        dest: shared.dest.clone(),
        limiter: shared.limiter.clone(),
        log: shared.log.clone(),
        active: shared.active.clone(),
        pool: UpstreamPool::new(if cfg.http.upstream_keepalive {
            cfg.http.max_idle_per_origin
        } else {
            0
        }),
    });

    loop {
        let (accepted, kind) = tokio::select! {
            a = plain.accept() => (a, Kind::Plain),
            a = accept_on(&tls_listener) => (a, Kind::Tls),
            a = accept_on(&socks_listener) => (a, Kind::Socks),
            _ = shutdown.changed() => break,
        };
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
        if !shared.clients.allows(ip) {
            tracing::debug!(%ip, "client not in allow_clients, closed");
            shared.log.refused(ip, Refusal::Allowlist);
            continue;
        }
        // Banned for repeated wrong keys: closed before any byte is read
        // (lock-free while nobody failed).
        if shared.limiter.banned(ip) {
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
                let Some(acceptor) = shared.tls.clone() else {
                    continue;
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    let hs = svc.cfg.tls.handshake_timeout;
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

/// CPUs this process may run on (honours Docker's cpuset), in order.
fn allowed_cpus() -> Vec<usize> {
    // SAFETY: cpu_set_t is plain data; sched_getaffinity fills it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return Vec::new();
        }
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&c| libc::CPU_ISSET(c, &set))
            .collect()
    }
}

/// Attaches a classic BPF program to the SO_REUSEPORT group of `listener`:
/// a connection whose SYN is processed on `cpus[i]` goes to the i-th socket
/// of the group (bind order = shard order). Other CPUs return an
/// out-of-range index, for which the kernel falls back to its hash.
fn steer_by_cpu(listener: &std::net::TcpListener, cpus: &[usize]) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let insn = |code: u32, jt: u8, jf: u8, k: u32| libc::sock_filter {
        code: code as u16,
        jt,
        jf,
        k,
    };
    // A = current CPU.
    let mut prog = vec![insn(
        libc::BPF_LD | libc::BPF_W | libc::BPF_ABS,
        0,
        0,
        (libc::SKF_AD_OFF + libc::SKF_AD_CPU) as u32,
    )];
    for (i, &cpu) in cpus.iter().enumerate() {
        // if A == cpu { return i } (else skip the return)
        prog.push(insn(
            libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
            0,
            1,
            cpu as u32,
        ));
        prog.push(insn(libc::BPF_RET | libc::BPF_K, 0, 0, i as u32));
    }
    prog.push(insn(libc::BPF_RET | libc::BPF_K, 0, 0, u32::MAX));
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_mut_ptr(),
    };
    // SAFETY: valid listening fd; `fprog` points into `prog`, alive for
    // the call (the kernel copies the program).
    let rc = unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ATTACH_REUSEPORT_CBPF,
            &fprog as *const libc::sock_fprog as *const libc::c_void,
            std::mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Pins the calling thread to `cpu` (best effort).
fn pin_to_cpu(cpu: usize) {
    // SAFETY: cpu_set_t is plain data; tid 0 = calling thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            tracing::warn!(cpu, "sched_setaffinity failed");
        }
    }
}

/// Health/admin listener: `GET /health` -> 200 "ok", anything else 404.
/// No auth, no proxying — separate port so probes never touch the proxy
/// path. 127.0.0.1-only by default on a public VPS.
async fn admin_loop(listener: TcpListener) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((sock, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(async move {
            let mut sock = sock;
            let _ = sock.set_nodelay(true);
            // Bounded probe read: request line + headers, 4 KB max, 2 s.
            let mut buf = vec![0u8; 4096];
            let mut filled = 0usize;
            while filled < buf.len() {
                let read =
                    tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf[filled..]))
                        .await;
                match read {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return,
                    Ok(Ok(n)) => {
                        filled += n;
                        if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let head = String::from_utf8_lossy(&buf[..filled]);
            let ok = head.starts_with("GET /health");
            let body: &[u8] = if ok { b"ok" } else { b"" };
            let status = if ok { "200 OK" } else { "404 Not Found" };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            if ok {
                let _ = sock.write_all(body).await;
            }
        });
    }
}

/// Resolves on SIGTERM or SIGINT.
pub async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {},
        _ = int.recv() => {},
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

/// Bind with a large accept backlog. Tokio/mio's default backlog is 1024,
/// which drops SYNs under connection bursts (benchmark: hold >= 5k showed
/// SYN retransmission latencies of 1s/3s/7s). `reuse_port`: one listener
/// per shard on the same address, the kernel balances between them.
fn bind(addr: SocketAddr, reuse_port: bool) -> anyhow::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    if reuse_port {
        socket.set_reuse_port(true)?;
    }
    // Inherited by every accepted socket (Linux clones the listener's
    // options): no setsockopt per connection. SO_RCVBUF before listen()
    // also sizes the window scale advertised in the SYN-ACK.
    socket.set_nodelay(true)?;
    // Every protocol served here (HTTP, TLS, SOCKS5) is client-speaks-first:
    // the kernel wakes accept() only once the first bytes are in, so the
    // first read never hits EAGAIN + an extra epoll round.
    let secs: libc::c_int = 1;
    // SAFETY: valid listening fd owned by `socket`, option value is a c_int
    // that outlives the call.
    unsafe {
        use std::os::unix::io::AsRawFd;
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_DEFER_ACCEPT,
            &secs as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    let buf = SOCK_BUF.load(Relaxed);
    if buf > 0 {
        let _ = socket.set_send_buffer_size(buf);
        let _ = socket.set_recv_buffer_size(buf);
    }
    socket.bind(&addr.into())?;
    // Capped by net.core.somaxconn (65535 in the benchmark harness).
    socket.listen(65535)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The CPU steering program is accepted by the kernel and a group
    /// with it still accepts connections (on any CPU: unknown ones fall
    /// back to the hash).
    #[test]
    fn cpu_steering_attaches() {
        use std::io::Write;
        let a = bind("127.0.0.1:0".parse().unwrap(), true).unwrap();
        let addr = a.local_addr().unwrap();
        let b = bind(addr, true).unwrap();
        steer_by_cpu(&a, &[0, 1]).unwrap();
        // A byte each: TCP_DEFER_ACCEPT holds silent connections back.
        let clients: Vec<_> = (0..8)
            .map(|_| {
                let mut c = std::net::TcpStream::connect(addr).unwrap();
                c.write_all(b"x").unwrap();
                c
            })
            .collect();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = 0;
        while got < clients.len() && std::time::Instant::now() < deadline {
            let before = got;
            got += [&a, &b].iter().filter(|l| l.accept().is_ok()).count();
            if got == before {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert_eq!(got, clients.len());
    }

    /// The per-connection setsockopts were dropped on the promise that
    /// accepted sockets inherit them from the listener: keep it checked.
    #[tokio::test]
    async fn accepted_socket_inherits_listener_options() {
        let ln =
            TcpListener::from_std(bind("127.0.0.1:0".parse().unwrap(), false).unwrap()).unwrap();
        let addr = ln.local_addr().unwrap();
        let _c = TcpStream::connect(addr).await.unwrap();
        let (s, _) = ln.accept().await.unwrap();
        assert!(s.nodelay().unwrap());
        let s = socket2::SockRef::from(&s);
        assert!(s.send_buffer_size().unwrap() >= SOCK_BUF.load(Relaxed));
        assert!(s.recv_buffer_size().unwrap() >= SOCK_BUF.load(Relaxed));
    }

    fn test_config(workers: usize, max_connections: usize) -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.server.listen = "127.0.0.1:0".to_string();
        cfg.server.admin_listen = String::new();
        cfg.server.workers = workers;
        cfg.limits.max_connections = max_connections;
        // Tests talk to origins on 127.0.0.1.
        cfg.filter.block_private = false;
        cfg.log.access = crate::accesslog::Level::Off;
        Arc::new(cfg)
    }

    /// Sends a request the proxy answers without an upstream (400) and
    /// returns the status line.
    async fn roundtrip(addr: SocketAddr, raw: &[u8]) -> String {
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(raw).await.unwrap();
        let mut out = Vec::new();
        c.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out)
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    }

    #[tokio::test]
    async fn shards_share_the_port_and_split_the_load() {
        let srv = start(test_config(2, 1000)).unwrap();
        let addr = srv.local_addr();
        for _ in 0..64 {
            let status = roundtrip(addr, b"garbage\r\n\r\n").await;
            assert!(status.starts_with("HTTP/1.1 400"), "{status}");
        }
        let per_shard: Vec<u64> = srv
            .shared
            .accepted
            .iter()
            .map(|a| a.0.load(Relaxed))
            .collect();
        assert_eq!(per_shard.iter().sum::<u64>(), 64);
        assert!(
            per_shard.iter().all(|&n| n > 0),
            "unbalanced: {per_shard:?}"
        );
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn max_connections_is_global_across_shards() {
        let srv = start(test_config(2, 2)).unwrap();
        let addr = srv.local_addr();
        // Two idle connections hold both slots (on whichever shards).
        let _a = TcpStream::connect(addr).await.unwrap();
        let _b = TcpStream::connect(addr).await.unwrap();
        while srv.shared.active.load(Relaxed) < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let status = roundtrip(addr, b"").await;
        assert!(status.starts_with("HTTP/1.1 503"), "{status}");
        drop((_a, _b));
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    // ---- HTTPS proxy listener (end to end through rustls) ----

    /// Self-signed cert for "localhost", written to a unique temp dir;
    /// returns (cert path, key path, DER for the client's trust store).
    fn tls_files() -> (String, String, rustls::pki_types::CertificateDer<'static>) {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "myproxy-tls-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (c, k) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&c, ck.cert.pem()).unwrap();
        std::fs::write(&k, ck.signing_key.serialize_pem()).unwrap();
        let der = ck.cert.der().clone();
        (
            c.to_string_lossy().into_owned(),
            k.to_string_lossy().into_owned(),
            der,
        )
    }

    async fn tls_connect(
        addr: SocketAddr,
        der: rustls::pki_types::CertificateDer<'static>,
    ) -> tokio_rustls::client::TlsStream<TcpStream> {
        tls_connect_alpn(addr, der, &[]).await
    }

    async fn tls_connect_alpn(
        addr: SocketAddr,
        der: rustls::pki_types::CertificateDer<'static>,
        alpn: &[&[u8]],
    ) -> tokio_rustls::client::TlsStream<TcpStream> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(der).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut cfg = cfg;
        cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        let conn = tokio_rustls::TlsConnector::from(Arc::new(cfg));
        let tcp = TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        conn.connect(name, tcp).await.unwrap()
    }

    /// Minimal keep-alive origin: `/chunked` answers chunked, anything
    /// else a 1000-byte Content-Length body.
    async fn origin() -> SocketAddr {
        let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = ln.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    loop {
                        let mut tmp = [0u8; 4096];
                        let n = match s.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                            buf.drain(..end + 4);
                            let resp: Vec<u8> = if head.contains("/chunked") {
                                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n".to_vec()
                            } else {
                                let mut r =
                                    b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n".to_vec();
                                r.extend(std::iter::repeat_n(b'x', 1000));
                                r
                            };
                            if s.write_all(&resp).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        addr
    }

    /// Reads one response (Content-Length or chunked, test-grade parser);
    /// returns (status line, body with chunked framing kept).
    async fn read_resp<S: tokio::io::AsyncRead + Unpin>(s: &mut S) -> (String, Vec<u8>) {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                let mut body = buf[end + 4..].to_vec();
                let status = head.lines().next().unwrap_or("").to_string();
                let lower = head.to_ascii_lowercase();
                let done = |b: &Vec<u8>| {
                    if lower.contains("transfer-encoding: chunked") {
                        b.ends_with(b"0\r\n\r\n")
                    } else {
                        let cl = lower
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .map(|v| v.trim().parse::<usize>().unwrap())
                            .unwrap_or(0);
                        b.len() >= cl
                    }
                };
                while !done(&body) {
                    let n = s.read(&mut tmp).await.unwrap();
                    assert!(n > 0, "EOF mid-body");
                    body.extend_from_slice(&tmp[..n]);
                }
                return (status, body);
            }
            let n = s.read(&mut tmp).await.unwrap();
            assert!(n > 0, "EOF before head");
            buf.extend_from_slice(&tmp[..n]);
        }
    }

    fn tls_config() -> (Arc<Config>, rustls::pki_types::CertificateDer<'static>) {
        let (cert, key, der) = tls_files();
        let mut cfg = (*test_config(2, 1000)).clone();
        cfg.tls.listen = "127.0.0.1:0".to_string();
        cfg.tls.cert = cert;
        cfg.tls.key = key;
        (Arc::new(cfg), der)
    }

    #[tokio::test]
    async fn https_proxy_serves_keepalive_http_and_chunked() {
        let (cfg, der) = tls_config();
        let mut cfg = (*cfg).clone();
        cfg.tls.ktls = true; // opt-in; this box has the `tls` module
        let up = origin().await;
        let srv = start(Arc::new(cfg)).unwrap();
        let mut c = tls_connect(srv.tls_addr().unwrap(), der).await;
        https_keepalive_round(&mut c, up).await;
        // The kernel took the session (module `tls` present on this box).
        assert_eq!(srv.ktls_sessions(), 1, "kTLS not engaged");
        drop(c);
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn https_proxy_userspace_tls_when_ktls_off() {
        let (cfg, der) = tls_config();
        let mut cfg = (*cfg).clone();
        cfg.tls.ktls = false;
        let up = origin().await;
        let srv = start(Arc::new(cfg)).unwrap();
        let mut c = tls_connect(srv.tls_addr().unwrap(), der).await;
        https_keepalive_round(&mut c, up).await;
        assert_eq!(srv.ktls_sessions(), 0);
        drop(c);
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    /// Keep-alive GETs (CL + chunked) and a flushed 400 over one TLS session.
    async fn https_keepalive_round<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        c: &mut S,
        up: SocketAddr,
    ) {
        for path in ["/a", "/chunked", "/b"] {
            let req = format!("GET http://{up}{path} HTTP/1.1\r\nHost: {up}\r\n\r\n");
            c.write_all(req.as_bytes()).await.unwrap();
            let (status, body) = read_resp(c).await;
            assert!(status.starts_with("HTTP/1.1 200"), "{path}: {status}");
            if path == "/chunked" {
                assert_eq!(body, b"5\r\nhello\r\n0\r\n\r\n");
            } else {
                assert_eq!(body.len(), 1000);
            }
        }
        // Errors are flushed through TLS too (not stuck in the session).
        c.write_all(b"garbage\r\n\r\n").await.unwrap();
        let (status, _) = read_resp(c).await;
        assert!(status.starts_with("HTTP/1.1 400"), "{status}");
    }

    #[tokio::test]
    async fn https_proxy_connect_tunnel() {
        let (cfg, der) = tls_config();
        // Echo server as the CONNECT target.
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = echo.accept().await.unwrap();
            let (mut r, mut w) = s.split();
            let _ = tokio::io::copy(&mut r, &mut w).await;
        });
        let srv = start(cfg).unwrap();
        let mut c = tls_connect(srv.tls_addr().unwrap(), der).await;
        let req = format!("CONNECT {echo_addr} HTTP/1.1\r\nHost: {echo_addr}\r\n\r\n");
        c.write_all(req.as_bytes()).await.unwrap();
        let mut head = vec![0u8; 39];
        c.read_exact(&mut head).await.unwrap();
        assert!(
            head.starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&head)
        );
        let payload: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
        let (mut r, mut w) = tokio::io::split(c);
        let p2 = payload.clone();
        let writer = tokio::spawn(async move {
            w.write_all(&p2).await.unwrap();
            w.flush().await.unwrap();
            w
        });
        let mut got = vec![0u8; payload.len()];
        r.read_exact(&mut got).await.unwrap();
        assert_eq!(got, payload);
        drop((r, writer.await.unwrap()));
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    // ---- HTTP/2 (ALPN h2 on the HTTPS listener) ----

    async fn h2_client(
        addr: SocketAddr,
        der: rustls::pki_types::CertificateDer<'static>,
    ) -> h2::client::SendRequest<bytes::Bytes> {
        let tls = tls_connect_alpn(addr, der, &[b"h2"]).await;
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
        let (send, conn) = h2::client::handshake(tls).await.unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });
        send
    }

    async fn h2_body(mut body: h2::RecvStream) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.unwrap();
            let _ = body.flow_control().release_capacity(chunk.len());
            out.extend_from_slice(&chunk);
        }
        out
    }

    #[tokio::test]
    async fn h2_get_content_length_and_chunked() {
        let (cfg, der) = tls_config();
        let up = origin().await;
        let srv = start(cfg).unwrap();
        let mut send = h2_client(srv.tls_addr().unwrap(), der).await;
        // Several streams on one connection, including a decoded chunked body.
        for (path, want) in [
            ("/a", vec![b'x'; 1000]),
            ("/chunked", b"hello".to_vec()),
            ("/b", vec![b'x'; 1000]),
        ] {
            let req = http::Request::get(format!("http://{up}{path}"))
                .body(())
                .unwrap();
            let (resp, _) = send.send_request(req, true).unwrap();
            let resp = resp.await.unwrap();
            assert_eq!(resp.status(), 200, "{path}");
            assert!(resp.headers().get("transfer-encoding").is_none());
            assert_eq!(h2_body(resp.into_body()).await, want, "{path}");
        }
        drop(send);
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn h2_connect_tunnel_and_auth() {
        let (cfg, der) = tls_config();
        let mut cfg = (*cfg).clone();
        cfg.auth.api_keys = vec!["k1".to_string()];
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = echo.accept().await.unwrap();
            let (mut r, mut w) = s.split();
            let _ = tokio::io::copy(&mut r, &mut w).await;
        });
        let srv = start(Arc::new(cfg)).unwrap();
        let mut send = h2_client(srv.tls_addr().unwrap(), der).await;

        // No credentials: 407 on the stream, the connection survives.
        let req = http::Request::builder()
            .method(http::Method::CONNECT)
            .uri(echo_addr.to_string())
            .body(())
            .unwrap();
        let (resp, _) = send.send_request(req, true).unwrap();
        assert_eq!(resp.await.unwrap().status(), 407);

        use base64::Engine as _;
        let cred = base64::engine::general_purpose::STANDARD.encode("u:k1");
        let req = http::Request::builder()
            .method(http::Method::CONNECT)
            .uri(echo_addr.to_string())
            .header("proxy-authorization", format!("Basic {cred}"))
            .body(())
            .unwrap();
        let (resp, mut tx) = send.send_request(req, false).unwrap();
        let resp = resp.await.unwrap();
        assert_eq!(resp.status(), 200);
        let mut rx = resp.into_body();
        let payload: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
        let p2 = payload.clone();
        let writer = tokio::spawn(async move {
            let mut data = bytes::Bytes::from(p2);
            while !data.is_empty() {
                tx.reserve_capacity(data.len());
                let cap = std::future::poll_fn(|cx| tx.poll_capacity(cx))
                    .await
                    .unwrap()
                    .unwrap();
                let chunk = data.split_to(cap.min(data.len()));
                tx.send_data(chunk, false).unwrap();
            }
            tx.send_data(bytes::Bytes::new(), true).unwrap();
        });
        let mut got = Vec::new();
        while got.len() < payload.len() {
            let chunk = rx.data().await.unwrap().unwrap();
            let _ = rx.flow_control().release_capacity(chunk.len());
            got.extend_from_slice(&chunk);
        }
        assert_eq!(got, payload);
        writer.await.unwrap();
        drop((rx, send));
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    // ---- access policy, bans, slowloris, SOCKS5 ----

    async fn send_recv(addr: SocketAddr, raw: &[u8]) -> Vec<u8> {
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(raw).await.unwrap();
        let mut out = Vec::new();
        let _ = c.read_to_end(&mut out).await;
        out
    }

    fn status(out: &[u8]) -> String {
        String::from_utf8_lossy(out)
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    }

    #[tokio::test]
    async fn destination_policy_and_port_allowlist() {
        let up = origin().await;
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.filter.block_private = true;
        let srv = start(Arc::new(cfg)).unwrap();
        let a = srv.local_addr();
        // Private destination (numeric and by name), CONNECT and plain GET.
        for req in [
            format!("CONNECT {up} HTTP/1.1\r\nHost: {up}\r\n\r\n"),
            format!("GET http://{up}/a HTTP/1.1\r\nHost: {up}\r\n\r\n"),
            format!(
                "GET http://localhost:{}/a HTTP/1.1\r\nHost: x\r\n\r\n",
                up.port()
            ),
            "CONNECT 169.254.169.254:80 HTTP/1.1\r\nHost: x\r\n\r\n".to_string(),
        ] {
            let st = status(&send_recv(a, req.as_bytes()).await);
            assert!(st.starts_with("HTTP/1.1 403"), "{req}: {st}");
        }
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();

        // Port allowlist (private filter off): allowed port passes, other refused.
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.filter.allowed_ports = vec![up.port()];
        let srv = start(Arc::new(cfg)).unwrap();
        let a = srv.local_addr();
        let ok = send_recv(
            a,
            format!("GET http://{up}/a HTTP/1.1\r\nHost: {up}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await;
        assert!(status(&ok).starts_with("HTTP/1.1 200"), "{}", status(&ok));
        let st = status(&send_recv(a, b"CONNECT 127.0.0.1:1 HTTP/1.1\r\nHost: x\r\n\r\n").await);
        assert!(st.starts_with("HTTP/1.1 403"), "{st}");
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn wrong_keys_ban_the_client_but_missing_credentials_do_not() {
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.auth.api_keys = vec!["good".into()];
        cfg.auth.max_failures = 3;
        let srv = start(Arc::new(cfg)).unwrap();
        let a = srv.local_addr();
        use base64::Engine as _;
        let bad = base64::engine::general_purpose::STANDARD.encode("u:bad");
        // No credentials: plain 407, never counted.
        for _ in 0..5 {
            let st = status(&send_recv(a, b"CONNECT x:1 HTTP/1.1\r\nHost: x\r\n\r\n").await);
            assert!(st.starts_with("HTTP/1.1 407"), "{st}");
        }
        for _ in 0..3 {
            let req = format!("CONNECT x:1 HTTP/1.1\r\nProxy-Authorization: Basic {bad}\r\n\r\n");
            let st = status(&send_recv(a, req.as_bytes()).await);
            assert!(st.starts_with("HTTP/1.1 407"), "{st}");
        }
        // Banned: the connection is closed without a response, even with
        // the good key.
        let good = base64::engine::general_purpose::STANDARD.encode("u:good");
        let req = format!("CONNECT x:1 HTTP/1.1\r\nProxy-Authorization: Basic {good}\r\n\r\n");
        assert!(send_recv(a, req.as_bytes()).await.is_empty());
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn slowloris_head_is_cut() {
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.timeouts.request_head = Duration::from_millis(300);
        let srv = start(Arc::new(cfg)).unwrap();
        let mut c = TcpStream::connect(srv.local_addr()).await.unwrap();
        let t0 = std::time::Instant::now();
        // One byte every 100 ms: each read is quick, the head never ends.
        let mut closed = false;
        for b in b"GET http://example.com/ HTTP/1.1\r\nX-Slow: aaaaaaaaaaaaaaaaaaaaaaaa".iter() {
            if c.write_all(&[*b]).await.is_err() {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut probe = [0u8; 1];
            if let Ok(Ok(0)) =
                tokio::time::timeout(Duration::from_millis(1), c.read(&mut probe)).await
            {
                closed = true;
                break;
            }
        }
        assert!(closed, "trickling client never cut");
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    /// SOCKS5 CONNECT with username/password; returns (stream, REP code).
    async fn socks_connect(
        addr: SocketAddr,
        pass: Option<&str>,
        target: SocketAddr,
    ) -> (TcpStream, u8) {
        let mut c = TcpStream::connect(addr).await.unwrap();
        let method = if pass.is_some() { 2 } else { 0 };
        c.write_all(&[5, 1, method]).await.unwrap();
        let mut sel = [0u8; 2];
        c.read_exact(&mut sel).await.unwrap();
        assert_eq!(sel, [5, method]);
        if let Some(p) = pass {
            let mut m = vec![1, 1, b'u', p.len() as u8];
            m.extend_from_slice(p.as_bytes());
            c.write_all(&m).await.unwrap();
            let mut st = [0u8; 2];
            c.read_exact(&mut st).await.unwrap();
            if st[1] != 0 {
                return (c, 0xff);
            }
        }
        let SocketAddr::V4(t) = target else { panic!() };
        let mut req = vec![5, 1, 0, 1];
        req.extend_from_slice(&t.ip().octets());
        req.extend_from_slice(&t.port().to_be_bytes());
        c.write_all(&req).await.unwrap();
        let mut rep = [0u8; 10];
        c.read_exact(&mut rep).await.unwrap();
        (c, rep[1])
    }

    #[tokio::test]
    async fn socks5_auth_tunnel_and_policy() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = echo.accept().await.unwrap();
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.auth.api_keys = vec!["k1".into()];
        cfg.socks5.listen = "127.0.0.1:0".into();
        let srv = start(Arc::new(cfg)).unwrap();
        let sa = srv.socks_addr().unwrap();

        let (_, rep) = socks_connect(sa, Some("nope"), echo_addr).await;
        assert_eq!(rep, 0xff, "wrong key accepted");

        let (mut c, rep) = socks_connect(sa, Some("k1"), echo_addr).await;
        assert_eq!(rep, 0);
        c.write_all(b"ping through socks").await.unwrap();
        let mut back = [0u8; 18];
        c.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping through socks");
        drop(c);
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();

        // Destination policy applies to SOCKS5 too (REP 0x02).
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.filter.block_private = true;
        cfg.socks5.listen = "127.0.0.1:0".into();
        let srv = start(Arc::new(cfg)).unwrap();
        let (_, rep) = socks_connect(srv.socks_addr().unwrap(), None, echo_addr).await;
        assert_eq!(rep, 0x02);
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn client_allowlist_closes_outsiders_at_accept() {
        // 127.0.0.1 outside the list: closed without a response, on the
        // HTTP and the SOCKS5 listeners.
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.auth.allow_clients = vec!["10.0.0.0/8".into()];
        cfg.socks5.listen = "127.0.0.1:0".into();
        let srv = start(Arc::new(cfg)).unwrap();
        assert!(send_recv(srv.local_addr(), b"garbage\r\n\r\n")
            .await
            .is_empty());
        assert!(send_recv(srv.socks_addr().unwrap(), &[5, 1, 0])
            .await
            .is_empty());
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();

        // Inside the list: served as usual.
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.auth.allow_clients = vec!["127.0.0.0/8".into()];
        let srv = start(Arc::new(cfg)).unwrap();
        let st = status(&send_recv(srv.local_addr(), b"garbage\r\n\r\n").await);
        assert!(st.starts_with("HTTP/1.1 400"), "{st}");
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    // ---- request bodies without Content-Length (chunked uploads) ----

    /// Origin that reads each request body (Content-Length or chunked)
    /// and answers "got <n>" (n = decoded body bytes).
    async fn upload_origin() -> SocketAddr {
        let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = ln.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut r = tokio::io::BufReader::new(s);
                    use tokio::io::AsyncBufReadExt;
                    loop {
                        let mut head = String::new();
                        loop {
                            let mut l = String::new();
                            if r.read_line(&mut l).await.unwrap_or(0) == 0 {
                                return;
                            }
                            if l == "\r\n" {
                                break;
                            }
                            head.push_str(&l.to_ascii_lowercase());
                        }
                        let mut n = 0usize;
                        if head.contains("transfer-encoding: chunked") {
                            loop {
                                let mut l = String::new();
                                r.read_line(&mut l).await.unwrap();
                                let size = usize::from_str_radix(l.trim(), 16).unwrap();
                                if size == 0 {
                                    r.read_line(&mut l).await.unwrap(); // final CRLF
                                    break;
                                }
                                let mut data = vec![0u8; size + 2];
                                r.read_exact(&mut data).await.unwrap();
                                n += size;
                            }
                        } else if let Some(cl) = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                        {
                            let cl: usize = cl.trim().parse().unwrap();
                            let mut data = vec![0u8; cl];
                            r.read_exact(&mut data).await.unwrap();
                            n = cl;
                        }
                        let body = format!("got {n}");
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        if r.get_mut().write_all(resp.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn chunked_upload_http1() {
        let up = upload_origin().await;
        let srv = start(test_config(1, 100)).unwrap();
        let mut c = TcpStream::connect(srv.local_addr()).await.unwrap();
        // Head and chunks in separate writes (framing split across reads).
        c.write_all(format!("POST http://{up}/u HTTP/1.1\r\nHost: {up}\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel").as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        c.write_all(b"lo\r\n6\r\n world\r\n").await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        c.write_all(b"0\r\n\r\n").await.unwrap();
        let (st, body) = read_resp(&mut c).await;
        assert!(st.starts_with("HTTP/1.1 200"), "{st}");
        assert_eq!(body, b"got 11");
        // Keep-alive survives: a Content-Length upload on the same connection.
        c.write_all(
            format!("POST http://{up}/u HTTP/1.1\r\nHost: {up}\r\nContent-Length: 3\r\n\r\nabc")
                .as_bytes(),
        )
        .await
        .unwrap();
        let (st, body) = read_resp(&mut c).await;
        assert!(st.starts_with("HTTP/1.1 200"), "{st}");
        assert_eq!(body, b"got 3");
        drop(c);
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn chunked_upload_over_budget_is_cut() {
        let up = upload_origin().await;
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.http.max_request_body_bytes = 8;
        let srv = start(Arc::new(cfg)).unwrap();
        let out = send_recv(
            srv.local_addr(),
            format!("POST http://{up}/u HTTP/1.1\r\nHost: {up}\r\nTransfer-Encoding: chunked\r\n\r\n10\r\n0123456789abcdef\r\n0\r\n\r\n").as_bytes(),
        )
        .await;
        assert!(
            !status(&out).starts_with("HTTP/1.1 200"),
            "{}",
            status(&out)
        );
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn h2_upload_without_length() {
        let (cfg, der) = tls_config();
        let up = upload_origin().await;
        let srv = start(cfg).unwrap();
        let mut send = h2_client(srv.tls_addr().unwrap(), der).await;
        let req = http::Request::post(format!("http://{up}/u"))
            .body(())
            .unwrap();
        let (resp, mut tx) = send.send_request(req, false).unwrap();
        tx.send_data(bytes::Bytes::from_static(b"hello "), false)
            .unwrap();
        tx.send_data(bytes::Bytes::from_static(b"h2 world"), true)
            .unwrap();
        let resp = resp.await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(h2_body(resp.into_body()).await, b"got 14");
        drop((tx, send));
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    // ---- SOCKS5 UDP ASSOCIATE ----

    /// TCP control connection + UDP ASSOCIATE; returns (control, relay addr).
    async fn socks_udp(addr: SocketAddr) -> (TcpStream, SocketAddr) {
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(&[5, 1, 0]).await.unwrap();
        let mut sel = [0u8; 2];
        c.read_exact(&mut sel).await.unwrap();
        c.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
        let mut rep = [0u8; 10];
        c.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[1], 0, "udp associate refused");
        let relay = SocketAddr::from((
            std::net::Ipv4Addr::new(rep[4], rep[5], rep[6], rep[7]),
            u16::from_be_bytes([rep[8], rep[9]]),
        ));
        (c, relay)
    }

    fn udp_dgram(to: SocketAddr, payload: &[u8]) -> Vec<u8> {
        let SocketAddr::V4(t) = to else { panic!() };
        let mut d = vec![0, 0, 0, 1];
        d.extend_from_slice(&t.ip().octets());
        d.extend_from_slice(&t.port().to_be_bytes());
        d.extend_from_slice(payload);
        d
    }

    async fn udp_echo() -> SocketAddr {
        let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let a = s.local_addr().unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 2048];
            loop {
                let (n, from) = s.recv_from(&mut b).await.unwrap();
                let _ = s.send_to(&b[..n], from).await;
            }
        });
        a
    }

    #[tokio::test]
    async fn socks5_udp_associate_relays_and_filters() {
        let echo = udp_echo().await;
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.socks5.listen = "127.0.0.1:0".into();
        let srv = start(Arc::new(cfg)).unwrap();
        let (ctl, relay) = socks_udp(srv.socks_addr().unwrap()).await;
        let u = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        u.send_to(&udp_dgram(echo, b"quic-ish"), relay)
            .await
            .unwrap();
        let mut b = [0u8; 2048];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), u.recv_from(&mut b))
            .await
            .expect("no udp reply")
            .unwrap();
        // Reply carries the SOCKS5 header with the echo server as source.
        assert_eq!(&b[..n], &udp_dgram(echo, b"quic-ish")[..]);
        // Closing the control connection ends the association.
        drop(ctl);
        tokio::time::sleep(Duration::from_millis(50)).await;
        u.send_to(&udp_dgram(echo, b"late"), relay).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), u.recv_from(&mut b))
                .await
                .is_err()
        );
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();

        // Destination policy: a private target is dropped.
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.socks5.listen = "127.0.0.1:0".into();
        cfg.filter.block_private = true;
        let srv = start(Arc::new(cfg)).unwrap();
        let (ctl, relay) = socks_udp(srv.socks_addr().unwrap()).await;
        u.send_to(&udp_dgram(echo, b"x"), relay).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), u.recv_from(&mut b))
                .await
                .is_err()
        );
        drop(ctl);
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    // ---- access log ----

    fn log_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("myproxy-access-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("access.log")
    }

    /// Waits (writer flushes every 200 ms) until the file has a line
    /// containing every needle; returns the file content.
    async fn wait_log(path: &std::path::Path, needles: &[&str]) -> String {
        for _ in 0..40 {
            let s = std::fs::read_to_string(path).unwrap_or_default();
            if s.lines().any(|l| needles.iter().all(|n| l.contains(n))) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "no line with {needles:?} in {}:\n{}",
            path.display(),
            std::fs::read_to_string(path).unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn access_log_all_requests_and_tunnels() {
        let up = origin().await;
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = echo.accept().await.unwrap();
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        let path = log_path("all");
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.log.access = crate::accesslog::Level::All;
        cfg.log.file = path.to_string_lossy().into_owned();
        cfg.log.target = crate::accesslog::TargetMode::Url;
        cfg.socks5.listen = "127.0.0.1:0".into();
        let srv = start(Arc::new(cfg)).unwrap();

        let out = send_recv(
            srv.local_addr(),
            format!("GET http://{up}/page?q=1 HTTP/1.1\r\nHost: {up}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await;
        assert!(status(&out).starts_with("HTTP/1.1 200"));
        wait_log(
            &path,
            &[
                "via=http1",
                "kind=http",
                "method=GET",
                &format!("target=http://{up}/page?q=1"),
                "status=200",
                "down=1000",
            ],
        )
        .await;

        let mut c = TcpStream::connect(srv.local_addr()).await.unwrap();
        c.write_all(format!("CONNECT {echo_addr} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut head = vec![0u8; 39];
        c.read_exact(&mut head).await.unwrap();
        c.write_all(b"12345").await.unwrap();
        let mut back = [0u8; 5];
        c.read_exact(&mut back).await.unwrap();
        drop(c);
        wait_log(
            &path,
            &[
                "kind=connect",
                &format!("target={echo_addr}"),
                "up=5",
                "down=5",
            ],
        )
        .await;

        let (mut c, rep) = socks_connect(srv.socks_addr().unwrap(), None, echo_addr).await;
        assert_eq!(rep, 0);
        c.write_all(b"abc").await.unwrap();
        let mut back = [0u8; 3];
        c.read_exact(&mut back).await.unwrap();
        drop(c);
        wait_log(&path, &["via=socks5", "kind=connect", "up=3", "down=3"]).await;
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn access_log_errors_only_and_reopen() {
        let up = origin().await;
        let path = log_path("errors");
        let mut cfg = (*test_config(1, 100)).clone();
        cfg.log.access = crate::accesslog::Level::Errors;
        cfg.log.file = path.to_string_lossy().into_owned();
        cfg.auth.api_keys = vec!["k".into()];
        cfg.filter.allowed_ports = vec![up.port()];
        let srv = start(Arc::new(cfg)).unwrap();
        let a = srv.local_addr();
        use base64::Engine as _;
        let good = base64::engine::general_purpose::STANDARD.encode("u:k");
        let bad = base64::engine::general_purpose::STANDARD.encode("u:x");

        // A 200 is not logged at `errors`.
        let ok = send_recv(a, format!("GET http://{up}/a HTTP/1.1\r\nProxy-Authorization: Basic {good}\r\nConnection: close\r\n\r\n").as_bytes()).await;
        assert!(status(&ok).starts_with("HTTP/1.1 200"));
        // Port refused and wrong key are.
        send_recv(
            a,
            format!("CONNECT 127.0.0.1:1 HTTP/1.1\r\nProxy-Authorization: Basic {good}\r\n\r\n")
                .as_bytes(),
        )
        .await;
        send_recv(
            a,
            format!("CONNECT x:1 HTTP/1.1\r\nProxy-Authorization: Basic {bad}\r\n\r\n").as_bytes(),
        )
        .await;
        let s = wait_log(&path, &["event=auth-fail"]).await;
        assert!(s.contains("status=403 reason=port"), "{s}");
        assert!(!s.contains("status=200"), "{s}");

        // logrotate: rename, reopen, the next line lands in the new file.
        let rotated = path.with_extension("log.1");
        std::fs::rename(&path, &rotated).unwrap();
        srv.reopen_logs();
        tokio::time::sleep(Duration::from_millis(300)).await;
        send_recv(a, b"CONNECT 127.0.0.1:2 HTTP/1.1\r\n\r\n").await; // 407 without creds: not logged
        send_recv(
            a,
            format!("CONNECT 127.0.0.1:2 HTTP/1.1\r\nProxy-Authorization: Basic {good}\r\n\r\n")
                .as_bytes(),
        )
        .await;
        wait_log(&path, &["target=127.0.0.1:2", "status=403"]).await;
        assert!(!std::fs::read_to_string(&rotated)
            .unwrap()
            .contains("127.0.0.1:2"));
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
    }
}
