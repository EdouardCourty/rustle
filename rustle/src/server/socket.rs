//! Listener sockets and socket buffers.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use anyhow::Context;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpStream;

/// Kernel socket buffer size (`server.socket_buffer_bytes`, 0 = autotuning).
/// One MB by default: the first in-flight burst before TCP autotuning
/// catches up was throttled by the 16 KB default (10 MB responses were
/// -11 % vs 3proxy). Applied best-effort on both sides of every proxied
/// socket. Kernel memory is allocated on demand (skb) — an idle connection
/// pays nothing. Process-wide: set once by `start`, read per upstream connect.
static SOCK_BUF: AtomicUsize = AtomicUsize::new(1024 * 1024);

/// Records the configured buffer size; warns when the kernel caps it (the
/// fixed size then both shrinks and disables autotuning: on a stock
/// rmem_max of 208 KB it bounds a 100 ms path to ~33 Mbit/s).
pub(super) fn set_socket_buffers(bytes: usize) {
    SOCK_BUF.store(bytes, Relaxed);
    #[cfg(not(target_os = "linux"))]
    let _ = bytes;
    #[cfg(target_os = "linux")]
    if bytes != 0 {
        warn_if_capped(bytes);
    }
}

/// `/proc/sys/net/core/{r,w}mem_max` below `bytes`: warn.
#[cfg(target_os = "linux")]
fn warn_if_capped(bytes: usize) {
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

/// Binds one listener per shard on `listen` (SO_REUSEPORT when sharded;
/// a `:0` port resolved by the first is shared by the others).
pub(super) fn bind_group(
    listen: &str,
    n: usize,
    what: &str,
) -> anyhow::Result<(SocketAddr, Vec<std::net::TcpListener>)> {
    let a: SocketAddr = listen
        .parse()
        .with_context(|| {
            format!("invalid {what} address {listen:?}: expected \"IP:port\", e.g. \"127.0.0.1:1080\" (literal IP, no host name)")
        })?;
    let first = bind(a, n > 1).with_context(|| format!("binding {what} {a}"))?;
    let addr = first.local_addr()?;
    let mut v = vec![first];
    for _ in 1..n {
        // Linux: one SO_REUSEPORT socket per shard, the kernel balances.
        // Elsewhere SO_REUSEPORT does not balance (BSD/macOS deliver to a
        // single socket): the shards share one listening socket instead.
        #[cfg(target_os = "linux")]
        v.push(bind(addr, true).with_context(|| format!("binding {what} {addr}"))?);
        #[cfg(not(target_os = "linux"))]
        v.push(
            v[0].try_clone()
                .with_context(|| format!("binding {what} {addr}"))?,
        );
    }
    Ok((addr, v))
}

/// Binds the control socket: refuses a live one (another instance), replaces
/// a stale one, restricts it to the owner.
pub(super) fn bind_control(
    path: &std::path::Path,
) -> anyhow::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    if path.exists() {
        anyhow::ensure!(
            UnixStream::connect(path).is_err(),
            "{} is in use by a running instance",
            path.display()
        );
        std::fs::remove_file(path).with_context(|| format!("removing stale {}", path.display()))?;
    }
    let l = UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    l.set_nonblocking(true)?;
    Ok(l)
}

/// Bind with a large accept backlog. Tokio/mio's default backlog is 1024,
/// which drops SYNs under connection bursts (benchmark: hold >= 5k showed
/// SYN retransmission latencies of 1s/3s/7s). `reuse_port`: one listener
/// per shard on the same address, the kernel balances between them.
pub(super) fn bind(addr: SocketAddr, reuse_port: bool) -> anyhow::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    #[cfg(target_os = "linux")]
    if reuse_port {
        socket.set_reuse_port(true)?;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = reuse_port;
    // Inherited by every accepted socket (Linux clones the listener's
    // options): no setsockopt per connection. SO_RCVBUF before listen()
    // also sizes the window scale advertised in the SYN-ACK.
    socket.set_nodelay(true)?;
    // Every protocol served here (HTTP, TLS, SOCKS5) is client-speaks-first:
    // the kernel wakes accept() only once the first bytes are in, so the
    // first read never hits EAGAIN + an extra epoll round.
    #[cfg(target_os = "linux")]
    {
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
