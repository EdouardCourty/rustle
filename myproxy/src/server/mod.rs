//! TCP acceptor: connection accounting, backpressure and dispatch to the
//! proxy pipeline. One acceptor loop, one task per client connection.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

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

/// Accept loop: enforce `max_connections` (semaphore, reject 503 above),
/// disable Nagle, widen kernel buffers, spawn one task per connection.
/// On SIGTERM/SIGINT: stop accepting and drain in-flight connections
/// (bounded to 5 s) — deploys no longer cut active clients.
pub async fn run(cfg: Arc<Config>) -> anyhow::Result<()> {
    let listener = bind(&cfg.server.listen)
        .await
        .context(format!("binding {}", cfg.server.listen))?;
    tracing::info!(
        addr = %cfg.server.listen,
        workers = cfg.server.workers,
        max_connections = cfg.limits.max_connections,
        "myproxy listening",
    );

    if !cfg.server.admin_listen.is_empty() {
        let admin = bind(&cfg.server.admin_listen)
            .await
            .context(format!("binding admin {}", cfg.server.admin_listen))?;
        tracing::info!(addr = %cfg.server.admin_listen, "admin/health listening");
        tokio::spawn(admin_loop(admin));
    }

    let slots = Arc::new(Semaphore::new(cfg.limits.max_connections));
    let auth = Arc::new(Authenticator::new(&cfg.auth.api_keys));
    let dns = Arc::new(DnsCache::new(cfg.dns.ttl, cfg.dns.max_entries));
    let pool = Arc::new(UpstreamPool::new(if cfg.http.upstream_keepalive {
        cfg.http.max_idle_per_origin
    } else {
        0
    }));
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    loop {
        let (sock, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    continue;
                }
            },
            _ = shutdown_signal() => {
                tracing::info!("shutdown signal: draining active connections");
                break;
            }
        };

        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tracing::warn!(?peer, "max_connections reached, rejecting");
            reject(sock);
            continue;
        };

        if let Err(e) = sock.set_nodelay(true) {
            tracing::warn!(?peer, error = %e, "set_nodelay failed");
        }
        widen_buffers(&sock);

        let cfg = cfg.clone();
        let auth = auth.clone();
        let dns = dns.clone();
        let pool = pool.clone();
        let active2 = active.clone();
        tokio::spawn(async move {
            let _permit = permit;
            proxy::handle_conn(sock, cfg, auth, dns, pool).await;
            active2.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });
        active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    // Graceful drain: let in-flight connections finish (capped to 5 s —
    // beyond that a deploy is more important than a stuck client).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while active.load(std::sync::atomic::Ordering::Relaxed) > 0
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tracing::info!(
        remaining = active.load(std::sync::atomic::Ordering::Relaxed),
        "shutdown complete"
    );
    Ok(())
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
async fn shutdown_signal() {
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
/// SYN retransmission latencies of 1s/3s/7s).
async fn bind(listen: &str) -> anyhow::Result<TcpListener> {
    let addr: std::net::SocketAddr = listen
        .parse()
        .with_context(|| format!("invalid listen address: {listen}"))?;
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    // Capped by net.core.somaxconn (65535 in the benchmark harness).
    socket.listen(65535)?;
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    Ok(TcpListener::from_std(std_listener)?)
}
