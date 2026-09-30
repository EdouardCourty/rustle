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

use crate::auth::Authenticator;
use crate::config::Config;
use crate::dns::DnsCache;
use crate::proxy;
use crate::upstream::UpstreamPool;

/// One MB of kernel socket buffer: the first in-flight burst before TCP
/// autotuning catches up was throttled by the 16 KB default (10 MB
/// responses were -11 % vs 3proxy). Applied best-effort on both sides of
/// every proxied socket. Kernel memory is allocated on demand (skb) — an
/// idle connection pays nothing.
const SOCK_BUF_BYTES: libc::c_int = 1024 * 1024;

/// Blocking-pool budget (getaddrinfo) split across shards.
const BLOCKING_THREADS_TOTAL: usize = 512;

/// Widen SO_SNDBUF/SO_RCVBUF on a socket (best effort, silent).
pub(crate) fn widen_buffers(sock: &TcpStream) {
    use std::os::unix::io::AsRawFd;
    let fd = sock.as_raw_fd();
    let buf: libc::c_int = SOCK_BUF_BYTES;
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
    /// HTTPS proxy listener's acceptor (None = plain only).
    tls: Option<tokio_rustls::TlsAcceptor>,
    /// Live client connections, all shards (enforces `max_connections`).
    active: AtomicUsize,
    /// High-water mark of `active` since the last heap trim.
    peak: AtomicUsize,
    /// Connections accepted per shard (balance check, logged at shutdown).
    accepted: Vec<AtomicU64>,
}

/// A running server: one thread per shard.
pub struct Server {
    addr: SocketAddr,
    tls_addr: Option<SocketAddr>,
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
pub fn start(cfg: Arc<Config>) -> anyhow::Result<Server> {
    let n = shard_count(cfg.server.workers);
    let listen: SocketAddr = cfg
        .server
        .listen
        .parse()
        .with_context(|| format!("invalid listen address: {}", cfg.server.listen))?;
    let reuse_port = n > 1;
    let first = bind(listen, reuse_port).with_context(|| format!("binding {listen}"))?;
    let addr = first.local_addr()?;
    let mut listeners = vec![first];
    for _ in 1..n {
        listeners.push(bind(addr, true).with_context(|| format!("binding {addr}"))?);
    }
    // HTTPS proxy listener: same sharding (one SO_REUSEPORT socket each).
    let (tls, tls_addr, mut tls_listeners) = if cfg.tls.listen.is_empty() {
        (None, None, Vec::new())
    } else {
        let acceptor = crate::tls::acceptor(&cfg.tls)?;
        let tl: SocketAddr = cfg
            .tls
            .listen
            .parse()
            .with_context(|| format!("invalid tls listen address: {}", cfg.tls.listen))?;
        let first = bind(tl, reuse_port).with_context(|| format!("binding tls {tl}"))?;
        let tl = first.local_addr()?;
        let mut v = vec![first];
        for _ in 1..n {
            v.push(bind(tl, true).with_context(|| format!("binding tls {tl}"))?);
        }
        (Some(acceptor), Some(tl), v)
    };
    let admin = if cfg.server.admin_listen.is_empty() {
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

    let shared = Arc::new(Shared {
        auth: Arc::new(Authenticator::new(&cfg.auth.api_keys)),
        dns: Arc::new(DnsCache::new(cfg.dns.ttl, cfg.dns.max_entries)),
        tls,
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        accepted: (0..n).map(|_| AtomicU64::new(0)).collect(),
        cfg,
    });
    tracing::info!(
        %addr,
        tls = ?tls_addr,
        shards = n,
        pinned = !cpus.is_empty(),
        max_connections = shared.cfg.limits.max_connections,
        "myproxy listening",
    );

    let (shutdown, rx) = watch::channel(false);
    let mut admin = admin;
    let mut shards = Vec::with_capacity(n);
    tls_listeners.reverse(); // popped in shard order below
    for (id, listener) in listeners.into_iter().enumerate() {
        let shared = shared.clone();
        let rx = rx.clone();
        let tls_listener = tls_listeners.pop();
        let admin = if id == 0 { admin.take() } else { None };
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
                rt.block_on(run_shard(id, shared, listener, tls_listener, admin, rx))
            })?;
        shards.push(handle);
    }
    Ok(Server {
        addr,
        tls_addr,
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

    /// HTTPS proxy listen address, when enabled.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn tls_addr(&self) -> Option<SocketAddr> {
        self.tls_addr
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
            .map(|a| a.load(Relaxed))
            .collect();
        tracing::info!(
            remaining = self.shared.active.load(Relaxed),
            accepted = ?per_shard,
            "shutdown complete"
        );
        result
    }
}

/// Decrements the global connection count when the connection task ends
/// (normal return or panic).
struct ConnSlot(Arc<Shared>);

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Relaxed);
    }
}

/// One shard: accept loop, one task per connection, its own upstream pool.
/// Shard 0 also serves the admin listener and the heap-trim task.
async fn run_shard(
    id: usize,
    shared: Arc<Shared>,
    listener: std::net::TcpListener,
    tls_listener: Option<std::net::TcpListener>,
    admin: Option<std::net::TcpListener>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let listener = TcpListener::from_std(listener)?;
    let tls_listener = tls_listener.map(TcpListener::from_std).transpose()?;
    if let Some(admin) = admin {
        tokio::spawn(admin_loop(TcpListener::from_std(admin)?));
    }
    #[cfg(all(
        target_env = "gnu",
        not(any(feature = "jemalloc", feature = "mimalloc"))
    ))]
    if id == 0 {
        tokio::spawn(trim_after_bursts(shared.clone()));
    }

    let cfg = shared.cfg.clone();
    let max_connections = cfg.limits.max_connections;
    let pool = Arc::new(UpstreamPool::new(if cfg.http.upstream_keepalive {
        cfg.http.max_idle_per_origin
    } else {
        0
    }));

    loop {
        let (accepted, is_tls) = tokio::select! {
            a = listener.accept() => (a, false),
            a = accept_on(&tls_listener) => (a, true),
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

        let now = shared.active.fetch_add(1, Relaxed) + 1;
        if now > max_connections {
            shared.active.fetch_sub(1, Relaxed);
            tracing::warn!(?peer, "max_connections reached, rejecting");
            reject(sock);
            continue;
        }
        shared.peak.fetch_max(now, Relaxed);
        shared.accepted[id].fetch_add(1, Relaxed);

        // TCP_NODELAY and the 1 MB buffers are inherited from the listener
        // (set once in `bind`): no per-connection setsockopt.
        let slot = ConnSlot(shared.clone());
        let cfg = cfg.clone();
        let auth = shared.auth.clone();
        let dns = shared.dns.clone();
        let pool = pool.clone();
        if is_tls {
            let Some(acceptor) = shared.tls.clone() else {
                continue;
            };
            tokio::spawn(async move {
                let _slot = slot;
                let hs = cfg.tls.handshake_timeout;
                match tokio::time::timeout(hs, acceptor.accept(sock)).await {
                    Ok(Ok(tls)) => {
                        if tls.get_ref().1.alpn_protocol() == Some(b"h2") {
                            proxy::handle_h2(tls, cfg, auth, dns, pool).await
                        } else {
                            proxy::handle_conn(tls, cfg, auth, dns, pool).await
                        }
                    }
                    Ok(Err(e)) => tracing::debug!(error = %e, "tls handshake failed"),
                    Err(_) => tracing::debug!("tls handshake timed out"),
                }
            });
        } else {
            tokio::spawn(async move {
                let _slot = slot;
                proxy::handle_conn(sock, cfg, auth, dns, pool).await;
            });
        }
    }

    // Graceful drain: let in-flight connections finish (capped to 5 s —
    // beyond that a deploy is more important than a stuck client). The
    // counter is global: every shard waits for the whole process.
    drop(listener);
    drop(tls_listener);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while shared.active.load(Relaxed) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
    let _ = socket.set_send_buffer_size(SOCK_BUF_BYTES as usize);
    let _ = socket.set_recv_buffer_size(SOCK_BUF_BYTES as usize);
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
        assert!(s.send_buffer_size().unwrap() >= SOCK_BUF_BYTES as usize);
        assert!(s.recv_buffer_size().unwrap() >= SOCK_BUF_BYTES as usize);
    }

    fn test_config(workers: usize, max_connections: usize) -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.server.listen = "127.0.0.1:0".to_string();
        cfg.server.admin_listen = String::new();
        cfg.server.workers = workers;
        cfg.limits.max_connections = max_connections;
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
            .map(|a| a.load(Relaxed))
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
        let up = origin().await;
        let srv = start(cfg).unwrap();
        let mut c = tls_connect(srv.tls_addr().unwrap(), der).await;
        for path in ["/a", "/chunked", "/b"] {
            let req = format!("GET http://{up}{path} HTTP/1.1\r\nHost: {up}\r\n\r\n");
            c.write_all(req.as_bytes()).await.unwrap();
            let (status, body) = read_resp(&mut c).await;
            assert!(status.starts_with("HTTP/1.1 200"), "{path}: {status}");
            if path == "/chunked" {
                assert_eq!(body, b"5\r\nhello\r\n0\r\n\r\n");
            } else {
                assert_eq!(body.len(), 1000);
            }
        }
        // Errors are flushed through TLS too (not stuck in the session).
        c.write_all(b"garbage\r\n\r\n").await.unwrap();
        let (status, _) = read_resp(&mut c).await;
        assert!(status.starts_with("HTTP/1.1 400"), "{status}");
        drop(c);
        tokio::task::spawn_blocking(move || srv.shutdown_and_join())
            .await
            .unwrap()
            .unwrap();
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
}
