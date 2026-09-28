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
    loop {
        let (mut s, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("sink: accept error: {e}");
                continue;
            }
        };
        tokio::spawn(async move {
            let _ = s.set_nodelay(true);
            let mut buf = [0u8; 65536];
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
    }
}
