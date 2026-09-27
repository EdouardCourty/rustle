mod auth;
mod config;
mod conn;
mod http;

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
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
    let listener = TcpListener::bind(&cfg.server.listen)
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
