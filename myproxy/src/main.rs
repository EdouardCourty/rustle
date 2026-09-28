mod auth;
mod config;
mod dns;
mod http;
mod proxy;
mod relay;
mod server;
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
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let cfg = Arc::new(Config::load(Path::new(&config_path)).context("loading config")?);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.server.workers.max(1))
        .enable_all()
        // Small tasks + a large accept fan-in: 128 KB/task stacks instead of
        // glibc/tokio defaults (2 MB reserve, ~12-20 KB touched per tunnel
        // just for handshakes). Cuts RSS/tunnel by an order of magnitude.
        .thread_stack_size(128 * 1024)
        .build()?;

    rt.block_on(server::run(cfg))
}
