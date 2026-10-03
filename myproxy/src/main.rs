mod accesslog;
mod auth;
mod config;
mod dns;
mod http;
mod ktls_offload;
mod policy;
mod proxy;
mod relay;
mod server;
mod stream;
mod tls;
mod upstream;

// Global allocator selection (compile-time feature; glibc by default).
// See myproxy/Cargo.toml [features]: jemalloc | mimalloc. The benchmark
// measures RSS with each — per-connection alloc/free churn keeps pages
// in RSS under glibc, the alternatives return them sooner.
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use config::Config;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        // Diagnostics on stderr: stdout may carry the access log. Colors
        // only on a terminal (journald / docker logs would keep the escapes).
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let cfg = Arc::new(Config::load(Path::new(&config_path)).context("loading config")?);

    // The main thread only waits for signals (tiny runtime, no I/O of its
    // own): SIGHUP reopens the access log, SIGTERM/SIGINT stop and join
    // the shards. Handlers are installed before the listeners open: a
    // signal right after startup must not hit the default action (exit).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _guard = rt.enter();
    use tokio::signal::unix::{signal, SignalKind};
    let mut hup = signal(SignalKind::hangup()).context("SIGHUP handler")?;
    let mut term = signal(SignalKind::terminate()).context("SIGTERM handler")?;
    let mut int = signal(SignalKind::interrupt()).context("SIGINT handler")?;
    let srv = server::start(cfg)?;
    rt.block_on(async {
        loop {
            tokio::select! {
                _ = term.recv() => break,
                _ = int.recv() => break,
                // logrotate: rename the file, then `kill -HUP`.
                _ = hup.recv() => srv.reopen_logs(),
            }
        }
    });
    tracing::info!("shutdown signal: draining active connections");
    srv.shutdown_and_join()
}
