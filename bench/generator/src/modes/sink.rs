//! TCP discard server (sink target): accepts connections and drops all
//! bytes. No process fork, no per-connection allocation (unlike socat).

use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

/// Bind with a large accept backlog (tokio/mio default is only 1024, which
/// drops SYNs under the hold test's connection bursts).
async fn bind_sink(listen: &str) -> anyhow::Result<TcpListener> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    let addr: std::net::SocketAddr = listen.parse()?;
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    let sa: SockAddr = addr.into();
    socket.bind(&sa)?;
    socket.listen(65535)?;
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    Ok(TcpListener::from_std(std_listener)?)
}

pub(crate) async fn run_sink(listen: &str) -> ! {
    let listener = match bind_sink(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("sink: bind {listen} failed: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("sink: listening on {listen}");
    // Reusable 8 KB read buffers: without the pool, each connection
    // allocates from the heap and glibc retains the pages after close —
    // at 25k tunnels the sink's heap grew past 1.5 GB (OOM-pressure on
    // the 4 GB box). With the pool the heap never grows: fixed 512 bufs
    // circulate, freed on task end.
    static POOL_SIZE: usize = 512;
    let pool = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::new()));
    loop {
        let (mut s, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("sink: accept error: {e}");
                continue;
            }
        };
        let pool = pool.clone();
        tokio::spawn(async move {
            let _ = s.set_nodelay(true);
            // Take a buffer from the pool (or allocate a fresh one).
            let mut buf = {
                let mut p = pool.lock().await;
                p.pop_front().unwrap_or_else(|| Vec::with_capacity(8192))
            };
            while let Ok(n) = s.read_buf(&mut buf).await {
                if n == 0 {
                    break; // EOF: connection closed
                }
                buf.clear(); // discard: keep the capacity
            }
            // Return the buffer to the pool (bounded).
            let mut p = pool.lock().await;
            if p.len() < POOL_SIZE {
                p.push_back(buf);
            }
        });
    }
}
