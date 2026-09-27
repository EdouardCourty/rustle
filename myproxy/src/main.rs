mod auth;
mod config;
mod conn;
mod http;

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing_subscriber::EnvFilter;

use auth::Authenticator;
use config::Config;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let cfg = Arc::new(Config::load(Path::new(&config_path)).context("loading config")?);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.server.workers.max(1))
        .enable_all()
        .build()?;

    rt.block_on(run(cfg))
}

async fn run(cfg: Arc<Config>) -> anyhow::Result<()> {
    let listener = bind(&cfg.server.listen)
        .await
        .with_context(|| format!("binding {}", cfg.server.listen))?;
    tracing::info!(
        addr = %cfg.server.listen,
        workers = cfg.server.workers,
        max_connections = cfg.limits.max_connections,
        "myproxy listening",
    );

    let slots = Arc::new(Semaphore::new(cfg.limits.max_connections));
    let auth = Arc::new(Authenticator::new(&cfg.auth.api_keys));

    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
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

        let cfg = cfg.clone();
        let auth = auth.clone();
        tokio::spawn(async move {
            let _permit = permit;
            conn::handle_conn(sock, cfg, auth).await;
        });
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
