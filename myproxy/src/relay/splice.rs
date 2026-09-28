//! Zero-copy relay (Linux): `splice()` moves data socket -> pipe -> socket
//! entirely in the kernel. Versus the adaptive relay this removes both
//! user-space copies per chunk; measured on this bench, 3proxy (which uses
//! the same technique) sustains 4.4 Gbps/core where read+write managed
//! 1.53 Gbps/core at equal saturation.

use std::os::unix::io::AsRawFd;

use tokio::io::unix::AsyncFd;
use tokio::net::TcpStream;

/// Kernel pipe buffer size for relay pipes. Default is 64 KB; enlarging
/// to 1 MB cuts the fill/drain iteration rate by ~16x on bulk flows
/// (10 MB responses: 45k -> 3k pump iterations/s) — measured as the last
/// CPU-side gap vs 3proxy on 10 MB objects (-13%). Requires root or
/// CAP_SYS_RESOURCE above `/proc/sys/fs/pipe-max-size`; on failure the
/// pipe keeps its default capacity (performance loss, not a breakage).
const PIPE_CAPACITY: libc::c_int = 1024 * 1024;

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

/// Bidirectional tunnel relay (both directions, half-close propagation).
///
/// Returns `Err` with the un-consumed streams when the zero-copy setup
/// fails (no pipes available): the caller falls back to the adaptive
/// engine without losing the sockets.
pub(super) async fn bidirectional(
    client: TcpStream,
    up: TcpStream,
) -> Result<(u64, u64), (TcpStream, TcpStream)> {
    let Some(p_ab) = sized_pipe() else {
        return Err((client, up));
    };
    let Some(p_ba) = sized_pipe() else {
        unsafe {
            libc::close(p_ab[0]);
            libc::close(p_ab[1]);
        }
        return Err((client, up));
    };
    let (pipe_ab_r, pipe_ab_w) = (PipeFd(p_ab[0]), PipeFd(p_ab[1]));
    let (pipe_ba_r, pipe_ba_w) = (PipeFd(p_ba[0]), PipeFd(p_ba[1]));

    // into_std() keeps O_NONBLOCK, which AsyncFd registration requires.
    let (client_std, up_std) = match (client.into_std(), up.into_std()) {
        (Ok(c), Ok(u)) => (c, u),
        _ => return Ok((0, 0)), // dead sockets: nothing was relayed
    };
    let (client_fd, up_fd) = match (AsyncFd::new(client_std), AsyncFd::new(up_std)) {
        (Ok(c), Ok(u)) => (c, u),
        _ => return Ok((0, 0)),
    };
    let (a2b, b2a) = tokio::join!(
        dir(&client_fd, &up_fd, &pipe_ab_r, &pipe_ab_w, true, None),
        dir(&up_fd, &client_fd, &pipe_ba_r, &pipe_ba_w, true, None)
    );
    Ok((a2b, b2a))
}

/// Zero-copy HTTP response relay. Dups both fds (the originals stay with
/// tokio: the client socket lives on for keep-alive, the upstream is
/// closed by the connection loop). A dup shares the file description, so
/// O_NONBLOCK and the underlying socket state are shared with the original.
///
/// Returns `None` when the zero-copy setup fails (caller keeps copying
/// adaptively).
pub(super) async fn response(
    up: &mut TcpStream,
    client: &mut TcpStream,
    limit: Option<u64>,
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
    let Some(p) = sized_pipe() else {
        unsafe {
            libc::close(up_fd_raw);
            libc::close(client_fd_raw);
        };
        return None;
    };
    let (pipe_r, pipe_w) = (PipeFd(p[0]), PipeFd(p[1]));

    // from_raw_fd objects own their fd and close it on drop: nothing leaks
    // below even when registration fails.
    let up_dup = unsafe { std::net::TcpStream::from_raw_fd(up_fd_raw) };
    let client_dup = unsafe { std::net::TcpStream::from_raw_fd(client_fd_raw) };
    let (up_fd, client_fd) = match (AsyncFd::new(up_dup), AsyncFd::new(client_dup)) {
        (Ok(u), Ok(c)) => (u, c),
        _ => return None,
    };
    // No half-close of the client on EOF: keep-alive must survive.
    Some(dir(&up_fd, &client_fd, &pipe_r, &pipe_w, false, limit).await)
}

/// RAII around a raw pipe fd so cancellation (idle timeout dropping the
/// relay future mid-flight) can never leak it.
struct PipeFd(std::ffi::c_int);
impl Drop for PipeFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// One direction of the zero-copy relay: `src` -> pipe -> `dst`.
/// Returns the number of bytes moved. On EOF from `src`, half-closes `dst`
/// when `shutdown_on_eof` (tunnels propagate half-close; the HTTP response
/// relay keeps the client open for keep-alive).
async fn dir(
    src: &AsyncFd<std::net::TcpStream>,
    dst: &AsyncFd<std::net::TcpStream>,
    pipe_r: &PipeFd,
    pipe_w: &PipeFd,
    shutdown_on_eof: bool,
    limit: Option<u64>,
) -> u64 {
    const SPLICE_F_MOVE: u32 = 1;
    const SPLICE_F_NONBLOCK: u32 = 2;
    let flags: u32 = SPLICE_F_MOVE | SPLICE_F_NONBLOCK;
    // Max bytes per src->pipe splice. NOT usize::MAX: the kernel rejects
    // len whose (ssize_t) cast is negative with EINVAL (found by probe).
    // The actual chunk is bounded by the pipe capacity (64 KB default).
    const SPLICE_CHUNK: usize = 256 * 1024;

    let mut moved: u64 = 0;
    let mut left: u64 = match limit {
        Some(l) => l,
        None => u64::MAX,
    };

    'outer: loop {
        if left == 0 {
            return moved; // body fully relayed, no over-splice
        }

        // Phase 1: wait for data on src and move up to one pipe-full.
        let mut in_pipe: usize;
        loop {
            let mut ready = match src.readable().await {
                Ok(g) => g,
                Err(_) => break 'outer, // reactor gone: bail out
            };
            let n = unsafe {
                libc::splice(
                    ready.get_inner().as_raw_fd(),
                    std::ptr::null_mut(),
                    pipe_w.0,
                    std::ptr::null_mut(),
                    SPLICE_CHUNK.min(left as usize),
                    flags,
                )
            };
            if n > 0 {
                in_pipe = n as usize;
                break; // got data: drain the pipe into dst
            }
            if n == 0 {
                // EOF from src: half-close dst so the far end sees it.
                if shutdown_on_eof {
                    unsafe {
                        libc::shutdown(dst.get_ref().as_raw_fd(), libc::SHUT_WR);
                    }
                }
                break 'outer;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                ready.clear_ready(); // spurious readiness: wait again
                continue;
            }
            break 'outer; // connection error: stop this direction
        }

        // Phase 2: drain the pipe into dst (still zero-copy).
        while in_pipe > 0 {
            let mut ready = match dst.writable().await {
                Ok(g) => g,
                Err(_) => break 'outer,
            };
            let n = unsafe {
                libc::splice(
                    pipe_r.0,
                    std::ptr::null_mut(),
                    ready.get_inner().as_raw_fd(),
                    std::ptr::null_mut(),
                    in_pipe,
                    flags,
                )
            };
            if n > 0 {
                in_pipe -= n as usize;
                moved += n as u64;
                left -= n as u64;
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
    }
    moved
}
