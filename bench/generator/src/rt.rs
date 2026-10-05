//! Shared-nothing worker threads: one `current_thread` runtime per thread,
//! no work stealing, no cross-thread wakeups on the hot path. A load
//! generator that bounces tasks between cores spends the CPU it was given
//! on scheduling instead of requests.

/// Runs `f(thread_index)` on `n` threads, each inside its own
/// `current_thread` runtime, and returns every thread's result.
pub(crate) fn run_threads<T, F, Fut>(n: usize, f: F) -> Vec<T>
where
    T: Send + 'static,
    F: Fn(usize) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = T>,
{
    let f = std::sync::Arc::new(f);
    let handles: Vec<_> = (0..n.max(1))
        .map(|i| {
            let f = f.clone();
            std::thread::Builder::new()
                .name(format!("bench-{i}"))
                .stack_size(256 * 1024)
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("runtime");
                    rt.block_on(f(i))
                })
                .expect("spawn")
        })
        .collect();
    handles
        .into_iter()
        .map(|h| h.join().expect("worker thread panicked"))
        .collect()
}

/// Connections of a total of `conns` that thread `i` of `n` owns.
pub(crate) fn share(conns: u32, n: usize, i: usize) -> u32 {
    let n = n.max(1) as u32;
    conns / n + u32::from((i as u32) < conns % n)
}

/// Binds a listening socket with a large backlog; `reuse_port` lets every
/// thread own its listener on the same address (kernel load balancing).
pub(crate) fn bind_listener(
    listen: &str,
    reuse_port: bool,
) -> anyhow::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    let addr: std::net::SocketAddr = listen.parse()?;
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    if reuse_port {
        socket.set_reuse_port(true)?;
    }
    #[cfg(not(unix))]
    let _ = reuse_port;
    socket.set_nodelay(true)?;
    let sa: SockAddr = addr.into();
    socket.bind(&sa)?;
    socket.listen(65535)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}
