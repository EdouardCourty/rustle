//! Zero-copy relay (Linux): `splice()` moves data socket -> pipe -> socket
//! entirely in the kernel. Versus the adaptive relay this removes both
//! user-space copies per chunk; measured on this bench, 3proxy (which uses
//! the same technique) sustains 4.4 Gbps/core where read+write managed
//! 1.53 Gbps/core at equal saturation.

use std::os::unix::io::AsRawFd;

use tokio::io::unix::AsyncFd;
use tokio::net::TcpStream;

use super::IdleTimer;

/// Kernel pipe buffer size for relay pipes. Default is 64 KB; enlarging
/// to 1 MB cuts the fill/drain iteration rate by ~16x on bulk flows
/// (10 MB responses: 45k -> 3k pump iterations/s) — measured as the last
/// CPU-side gap vs 3proxy on 10 MB objects (-13%). Requires root or
/// CAP_SYS_RESOURCE above `/proc/sys/fs/pipe-max-size`; on failure the
/// pipe keeps its default capacity (performance loss, not a breakage).
const PIPE_CAPACITY: libc::c_int = 1024 * 1024;

/// One fill call may request up to the pipe capacity; the kernel caps at
/// the pipe's free space anyway.
const SPLICE_CHUNK: usize = PIPE_CAPACITY as usize;

/// Create a non-blocking pipe sized to [`PIPE_CAPACITY`] (`F_SETPIPE_SZ`,
/// best effort): returns `None` and no allocations on failure.
fn sized_pipe() -> Option<[std::ffi::c_int; 2]> {
    let mut p = [0 as std::ffi::c_int; 2];
    if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
        return None;
    }
    unsafe {
        // Best effort: the kernel may clamp to fs.pipe-max-size; failures
        // keep the default 64 KB capacity, which still works correctly.
        libc::fcntl(p[0], libc::F_SETPIPE_SZ, PIPE_CAPACITY);
        libc::fcntl(p[1], libc::F_SETPIPE_SZ, PIPE_CAPACITY);
    }
    Some(p)
}

/// RAII around a raw pipe fd so cancellation (idle timeout dropping the
/// relay future mid-flight) can never leak it.
struct PipeFd(std::ffi::c_int);
impl Drop for PipeFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Reusable pipe pair for HTTP keep-alive connections: created once (on
/// the first bulk response) and reused across responses, saving ~6
/// syscalls per request (`pipe2` + `F_SETPIPE_SZ` + teardown). The pair
/// is owned by the connection FSM and dropped with it.
pub struct PipePair {
    r: PipeFd,
    w: PipeFd,
}

impl PipePair {
    /// Sized pipe pair, or `None` when the kernel refuses (the caller
    /// keeps the adaptive engine — performance loss only).
    pub fn new() -> Option<Self> {
        let Some(p) = sized_pipe() else {
            return None;
        };
        Some(Self {
            r: PipeFd(p[0]),
            w: PipeFd(p[1]),
        })
    }
}

/// Bidirectional tunnel relay (both directions, half-close propagation).
///
/// Returns `Err` with the un-consumed streams when the zero-copy setup
/// fails (no pipes available): the caller falls back to the adaptive
/// engine without losing the sockets.
pub(super) async fn bidirectional(
    client: TcpStream,
    up: TcpStream,
    idle: std::time::Duration,
) -> Result<(u64, u64), (TcpStream, TcpStream)> {
    let Some(pair_ab) = PipePair::new() else {
        return Err((client, up));
    };
    let Some(pair_ba) = PipePair::new() else {
        return Err((client, up)); // pair_ab's raw fds closed by its Drop
    };

    // into_std() keeps O_NONBLOCK, which AsyncFd registration requires.
    let (client_std, up_std) = match (client.into_std(), up.into_std()) {
        (Ok(c), Ok(u)) => (c, u),
        _ => return Ok((0, 0)), // dead sockets: nothing was relayed
    };
    let (client_fd, up_fd) = match (AsyncFd::new(client_std), AsyncFd::new(up_std)) {
        (Ok(c), Ok(u)) => (c, u),
        _ => return Ok((0, 0)),
    };
    let mut timer_a = IdleTimer::new(idle);
    let mut timer_b = IdleTimer::new(idle);
    let (a2b, b2a) = tokio::join!(
        dir(
            &client_fd,
            &up_fd,
            &pair_ab.r,
            &pair_ab.w,
            true,
            None,
            &mut timer_a
        ),
        dir(
            &up_fd,
            &client_fd,
            &pair_ba.r,
            &pair_ba.w,
            true,
            None,
            &mut timer_b
        )
    );
    Ok((a2b, b2a))
}

/// Zero-copy HTTP response relay. Dups both fds (the originals stay with
/// tokio: the client socket lives on for keep-alive, the upstream is
/// closed by the connection loop). A dup shares the file description, so
/// O_NONBLOCK and the underlying socket state are shared with the
/// original.
///
/// Uses the caller's reusable [`PipePair`]. Returns `None` when the
/// zero-copy setup fails (caller keeps copying adaptively — and should
/// consider the pair dirty/unknown only if the relay already started,
/// which cannot happen before `Some` is returned).
pub(super) async fn response(
    up: &mut TcpStream,
    client: &mut TcpStream,
    limit: Option<u64>,
    pair: &PipePair,
    idle: std::time::Duration,
) -> Option<u64> {
    use std::os::unix::io::FromRawFd;

    let up_fd_raw = unsafe { libc::dup(up.as_raw_fd()) };
    if up_fd_raw < 0 {
        return None;
    }
    let client_fd_raw = unsafe { libc::dup(client.as_raw_fd()) };
    if client_fd_raw < 0 {
        unsafe { libc::close(up_fd_raw) };
        return None;
    }
    // from_raw_fd objects own their fd and close it on drop: nothing leaks
    // below even when registration fails.
    let up_dup = unsafe { std::net::TcpStream::from_raw_fd(up_fd_raw) };
    let client_dup = unsafe { std::net::TcpStream::from_raw_fd(client_fd_raw) };
    let (up_fd, client_fd) = match (AsyncFd::new(up_dup), AsyncFd::new(client_dup)) {
        (Ok(u), Ok(c)) => (u, c),
        _ => return None,
    };
    // No half-close of the client on EOF: keep-alive must survive.
    let mut timer = IdleTimer::new(idle);
    Some(
        dir(
            &up_fd, &client_fd, &pair.r, &pair.w, false, limit, &mut timer,
        )
        .await,
    )
}

/// One direction of the zero-copy relay: `src` -> pipe -> `dst`.
/// Returns the number of bytes moved. On EOF from `src`, half-closes `dst`
/// when `shutdown_on_eof` (tunnels propagate half-close; the HTTP response
/// relay keeps the client open for keep-alive).
///
/// Fill-then-drain: the pipe is filled to capacity (or socket-drained)
/// before each drain, minimizing readable/writable wait pairs per byte.
/// An attempt is also made *before* any readiness wait — under sustained
/// bulk the socket is already readable and one park per MB is saved.
async fn dir(
    src: &AsyncFd<std::net::TcpStream>,
    dst: &AsyncFd<std::net::TcpStream>,
    pipe_r: &PipeFd,
    pipe_w: &PipeFd,
    shutdown_on_eof: bool,
    limit: Option<u64>,
    timer: &mut IdleTimer,
) -> u64 {
    const SPLICE_F_MOVE: u32 = 1;
    const SPLICE_F_NONBLOCK: u32 = 2;
    let flags: u32 = SPLICE_F_MOVE | SPLICE_F_NONBLOCK;

    let src_fd = src.get_ref().as_raw_fd();
    let dst_fd = dst.get_ref().as_raw_fd();
    let mut moved: u64 = 0;
    let mut left: u64 = match limit {
        Some(l) => l,
        None => u64::MAX,
    };

    'outer: loop {
        if left == 0 {
            return moved; // body fully relayed, no over-splice
        }

        // Phase 1: FILL the pipe (kernel copies only, no user-space turn).
        let mut in_pipe: usize = 0;
        let mut pending_eof = false;
        loop {
            let want = SPLICE_CHUNK.min(left as usize).saturating_sub(in_pipe);
            if want == 0 {
                break; // pipe full or limit reached: go drain
            }
            // Attempt 1: without any readiness wait (data often queued).
            let n = unsafe {
                libc::splice(
                    src_fd,
                    std::ptr::null_mut(),
                    pipe_w.0,
                    std::ptr::null_mut(),
                    want,
                    flags,
                )
            };
            if n > 0 {
                in_pipe += n as usize;
                timer.reset();
                continue; // keep filling without parking
            }
            if n == 0 {
                pending_eof = true;
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::WouldBlock {
                break 'outer; // connection error: stop this direction
            }
            // Socket dry. CRITICAL: if the pipe holds data, DRAIN it now —
            // parking on src.readable() with a loaded pipe deadlocks the
            // relay (measured: upstream waits for our window, client waits
            // for our pipe, 0 % CPU everywhere). Park only when empty.
            if in_pipe > 0 {
                break; // go drain the pipe into dst
            }
            // Socket empty: park until readable (inactivity-bounded),
            // then one guarded attempt.
            let mut ready = match timer.park(src.readable()).await {
                Some(Ok(g)) => g,
                _ => break 'outer, // reactor gone or idle timeout
            };
            let n = unsafe {
                libc::splice(
                    src_fd,
                    std::ptr::null_mut(),
                    pipe_w.0,
                    std::ptr::null_mut(),
                    want,
                    flags,
                )
            };
            if n > 0 {
                in_pipe += n as usize;
                break; // got data: go drain the pipe into dst
            }
            if n == 0 {
                pending_eof = true;
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                ready.clear_ready(); // spurious readiness: park again
                continue;
            }
            break 'outer; // connection error: stop this direction
        }

        // Phase 2: drain the pipe into dst (still zero-copy).
        while in_pipe > 0 {
            let mut ready = match timer.park(dst.writable()).await {
                Some(Ok(g)) => g,
                _ => break 'outer, // inactivity or reactor gone
            };
            let n = unsafe {
                libc::splice(
                    pipe_r.0,
                    std::ptr::null_mut(),
                    dst_fd,
                    std::ptr::null_mut(),
                    in_pipe,
                    flags,
                )
            };
            if n > 0 {
                in_pipe -= n as usize;
                moved += n as u64;
                left -= n as u64;
                timer.reset();
            } else if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    ready.clear_ready();
                    continue;
                }
                break 'outer;
            } else {
                break 'outer; // n == 0: dst closed for writing
            }
        }

        if pending_eof {
            // EOF from src after draining the last pipe-full: half-close
            // dst so the far end sees it (when allowed).
            if shutdown_on_eof {
                unsafe {
                    libc::shutdown(dst_fd, libc::SHUT_WR);
                }
            }
            return moved;
        }
    }
    moved
}
