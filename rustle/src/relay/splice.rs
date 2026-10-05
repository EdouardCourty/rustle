//! Zero-copy relay (Linux): `splice()` moves data socket -> pipe -> socket
//! entirely in the kernel, both user-space copies per chunk gone.
//!
//! - Pipes come from a per-shard pool (thread-local, bounded) and go back
//!   only when empty. A pipe is 1 MB when the kernel allows it; the
//!   per-user pipe budget (`fs.pipe-user-pages-soft`, ~60 pipes of 1 MB
//!   without CAP_SYS_RESOURCE) silently shrinks further pipes to 8 KB —
//!   worse than copying — so a pipe below 64 KB is refused and the relay
//!   keeps copying (counted: [`degraded_pipes`]).
//! - Tunnels start copying and take a pipe only once a direction proves
//!   bulk: an idle tunnel holds no pipe at all.
//! - Splice runs on the tokio sockets themselves (`try_io`): no `dup`, no
//!   second epoll registration, readiness cleared by tokio on EAGAIN.

use std::cell::RefCell;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use tokio::io::Interest;
use tokio::net::TcpStream;

use super::{IdleTimer, RELAY_BUF0, RELAY_BUF_MAX};

/// Wanted pipe capacity: one fill/drain pair per MB of bulk.
const PIPE_CAPACITY: libc::c_int = 1024 * 1024;
/// Below this, splice costs more syscalls than the 64 KB copy relay.
const PIPE_MIN_USEFUL: libc::c_int = 64 * 1024;
/// Idle pipes kept per shard.
const POOL_MAX: usize = 16;

thread_local! {
    static POOL: RefCell<Vec<PipePair>> = const { RefCell::new(Vec::new()) };
}

/// Pipes refused because the kernel would not size them (budget hit).
static DEGRADED: AtomicU64 = AtomicU64::new(0);

/// Pipes refused so far (pipe budget exhausted): logged at shutdown.
pub fn degraded_pipes() -> u64 {
    DEGRADED.load(Relaxed)
}

/// A non-blocking pipe sized for bulk relaying, pooled per shard.
pub struct PipePair {
    r: libc::c_int,
    w: libc::c_int,
    cap: usize,
}

impl PipePair {
    /// A pooled pipe, or a fresh one; `None` when the kernel cannot give a
    /// useful size (the caller keeps copying).
    pub fn take() -> Option<Self> {
        if let Some(p) = POOL.with(|p| p.borrow_mut().pop()) {
            return Some(p);
        }
        Self::create()
    }

    fn create() -> Option<Self> {
        let mut p = [0 as libc::c_int; 2];
        // SAFETY: plain pipe2/fcntl/close on fds we own.
        unsafe {
            if libc::pipe2(p.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) != 0 {
                return None;
            }
            // One call (both ends share the pipe); on refusal, read back
            // what the kernel actually gave.
            let mut cap = libc::fcntl(p[1], libc::F_SETPIPE_SZ, PIPE_CAPACITY);
            if cap < 0 {
                cap = libc::fcntl(p[1], libc::F_GETPIPE_SZ);
            }
            if cap < PIPE_MIN_USEFUL {
                DEGRADED.fetch_add(1, Relaxed);
                libc::close(p[0]);
                libc::close(p[1]);
                return None;
            }
            Some(Self {
                r: p[0],
                w: p[1],
                cap: cap as usize,
            })
        }
    }
}

impl Drop for PipePair {
    fn drop(&mut self) {
        // Back to the pool only when empty (an aborted relay may leave
        // bytes in it); never during thread teardown.
        let mut queued: libc::c_int = 0;
        // SAFETY: FIONREAD into a c_int on our own fd.
        let empty = unsafe { libc::ioctl(self.r, libc::FIONREAD, &mut queued) } == 0 && queued == 0;
        if empty {
            // The slot is borrowed, never moved into the closure: if the
            // pool is already gone (thread teardown) nothing is dropped
            // inside `try_with` (that would recurse into this Drop).
            let mut slot = Some(PipePair {
                r: self.r,
                w: self.w,
                cap: self.cap,
            });
            let pooled = POOL
                .try_with(|p| {
                    let mut p = p.borrow_mut();
                    if p.len() < POOL_MAX {
                        p.extend(slot.take());
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if pooled {
                return;
            }
            if let Some(p) = slot {
                std::mem::forget(p); // same fds as `self`: closed below, once
            }
        }
        // SAFETY: closing fds we own, once.
        unsafe {
            libc::close(self.r);
            libc::close(self.w);
        }
    }
}

fn splice(from: libc::c_int, to: libc::c_int, len: usize) -> std::io::Result<usize> {
    const SPLICE_F_MOVE: libc::c_uint = 1;
    const SPLICE_F_NONBLOCK: libc::c_uint = 2;
    // SAFETY: splice between fds we hold, no offsets.
    let n = unsafe {
        libc::splice(
            from,
            std::ptr::null_mut(),
            to,
            std::ptr::null_mut(),
            len,
            SPLICE_F_MOVE | SPLICE_F_NONBLOCK,
        )
    };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// Bulk HTTP response, upstream -> client, through `pipe` (no half-close:
/// the client connection lives on for keep-alive).
pub(super) async fn response(
    up: &TcpStream,
    client: &TcpStream,
    limit: Option<u64>,
    pipe: &PipePair,
    idle: std::time::Duration,
) -> u64 {
    let mut timer = IdleTimer::new(idle);
    dir(up, client, pipe, Eof::Keep, limit, &mut timer).await
}

/// Tunnel relay, both directions, half-close propagated. `client_ktls`:
/// the client socket carries a kTLS session, whose end is a close_notify
/// record before the FIN.
pub(super) async fn tunnel(
    client: TcpStream,
    up: TcpStream,
    idle: std::time::Duration,
    client_ktls: bool,
) -> (u64, u64) {
    tokio::join!(
        tunnel_dir(&client, &up, idle, false),
        tunnel_dir(&up, &client, idle, client_ktls)
    )
}

/// One tunnel direction: adaptive copy (8 -> 64 KB) until the flow proves
/// bulk, then splice through a pooled pipe for the rest.
async fn tunnel_dir(
    src: &TcpStream,
    dst: &TcpStream,
    idle: std::time::Duration,
    dst_ktls: bool,
) -> u64 {
    let mut timer = IdleTimer::new(idle);
    // Allocated only once data is ready: an idle tunnel (most of them)
    // holds no buffer at all.
    let mut buf: Vec<u8> = Vec::new();
    let mut moved: u64 = 0;
    loop {
        if buf.capacity() == 0 {
            match timer.park(src.readable()).await {
                Some(Ok(())) => buf.reserve_exact(RELAY_BUF0),
                _ => return moved,
            }
        }
        buf.clear();
        let n = loop {
            match src.try_read_buf(&mut buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    match timer.park(src.readable()).await {
                        Some(Ok(())) => {}
                        _ => return moved, // inactivity / reactor gone
                    }
                }
                Err(_) => break 0, // reset: ends this direction like EOF
            }
        };
        if n == 0 {
            half_close(dst, dst_ktls);
            return moved;
        }
        let mut off = 0;
        while off < n {
            match dst.try_write(&buf[off..n]) {
                Ok(k) => off += k,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    match timer.park(dst.writable()).await {
                        Some(Ok(())) => {}
                        _ => return moved,
                    }
                }
                Err(_) => return moved,
            }
        }
        moved += n as u64;
        timer.reset();
        if n == buf.capacity() {
            if buf.capacity() < RELAY_BUF_MAX {
                buf.reserve(buf.capacity()); // bulk: grow
            } else if let Some(pipe) = PipePair::take() {
                drop(buf);
                let eof = Eof::HalfClose { ktls: dst_ktls };
                return moved + dir(src, dst, &pipe, eof, None, &mut timer).await;
            }
        }
    }
}

fn half_close(s: &TcpStream, ktls: bool) {
    if ktls {
        let _ = crate::ktls_offload::send_close_notify(s.as_raw_fd());
    }
    // SAFETY: shutdown(SHUT_WR) on a socket we hold.
    unsafe {
        libc::shutdown(s.as_raw_fd(), libc::SHUT_WR);
    }
}

/// What a direction does with `dst` on EOF from `src`.
#[derive(Clone, Copy)]
enum Eof {
    /// HTTP response relay: the client connection lives on.
    Keep,
    /// Tunnel: half-close `dst` (close_notify first on a kTLS session).
    HalfClose { ktls: bool },
}

/// One direction of the zero-copy relay: `src` -> pipe -> `dst`, at most
/// `limit` bytes. On EOF from `src`, `on_eof` says whether to half-close
/// `dst`. Returns the bytes moved.
///
/// Fill-then-drain: the pipe is filled until the socket is dry (a short
/// splice says so — no extra EAGAIN probe) or the pipe full, then drained.
/// Invariant: never park on `src.readable()` while the pipe holds data
/// (the far end may be waiting for exactly those bytes: deadlock).
async fn dir(
    src: &TcpStream,
    dst: &TcpStream,
    pipe: &PipePair,
    on_eof: Eof,
    limit: Option<u64>,
    timer: &mut IdleTimer,
) -> u64 {
    let src_fd = src.as_raw_fd();
    let dst_fd = dst.as_raw_fd();
    let mut moved: u64 = 0;
    let mut left: u64 = limit.unwrap_or(u64::MAX);
    'outer: loop {
        if left == 0 {
            return moved; // body fully relayed, no over-splice
        }
        // Phase 1: fill. `want` never exceeds the free pipe space, so an
        // EAGAIN means the socket is dry (tokio then clears its readiness).
        let mut in_pipe: usize = 0;
        let mut eof = false;
        loop {
            let want = (pipe.cap as u64).min(left) as usize - in_pipe;
            if want == 0 {
                break;
            }
            match src.try_io(Interest::READABLE, || splice(src_fd, pipe.w, want)) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => {
                    in_pipe += n;
                    timer.reset();
                    if n < want {
                        break; // socket drained (or pipe slots full): drain now
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if in_pipe > 0 {
                        break; // drain what we hold before waiting (invariant)
                    }
                    match timer.park(src.readable()).await {
                        Some(Ok(())) => {}
                        _ => break 'outer,
                    }
                }
                // Reset, or a kTLS control record (close_notify: EIO): ends
                // this direction like EOF, as in the copy path — what the
                // pipe holds is still delivered, then `on_eof` applies.
                Err(_) => {
                    eof = true;
                    break;
                }
            }
        }
        // Phase 2: drain into dst.
        while in_pipe > 0 {
            match dst.try_io(Interest::WRITABLE, || splice(pipe.r, dst_fd, in_pipe)) {
                Ok(0) => break 'outer, // dst closed for writing
                Ok(n) => {
                    in_pipe -= n;
                    moved += n as u64;
                    left -= n as u64;
                    timer.reset();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    match timer.park(dst.writable()).await {
                        Some(Ok(())) => {}
                        _ => break 'outer,
                    }
                }
                Err(_) => break 'outer,
            }
        }
        if eof {
            if let Eof::HalfClose { ktls } = on_eof {
                half_close(dst, ktls);
            }
            return moved;
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pooled() -> usize {
        POOL.with(|p| p.borrow().len())
    }

    #[test]
    fn empty_pipes_return_to_the_pool_dirty_ones_do_not() {
        let base = pooled();
        let a = PipePair::take().expect("pipe");
        assert!(a.cap >= PIPE_MIN_USEFUL as usize);
        drop(a);
        assert_eq!(pooled(), base + 1, "empty pipe pooled");
        // Reused, not recreated.
        let b = PipePair::take().unwrap();
        assert_eq!(pooled(), base);
        // Bytes left inside (aborted relay): closed, not pooled.
        // SAFETY: write into our own pipe.
        unsafe { libc::write(b.w, b"x".as_ptr().cast(), 1) };
        drop(b);
        assert_eq!(pooled(), base);
    }

    #[test]
    fn pool_is_bounded() {
        let pipes: Vec<_> = (0..POOL_MAX + 4).filter_map(|_| PipePair::take()).collect();
        let n = pipes.len();
        drop(pipes);
        assert!(
            pooled() <= POOL_MAX,
            "pool grew past its cap ({n} pipes made)"
        );
    }
}
