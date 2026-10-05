mod accesslog;
mod auth;
mod cli;
mod cmd;
mod config;
mod dns;
mod http;
#[cfg(target_os = "linux")]
mod ktls_offload;
mod policy;
mod proxy;
mod relay;
mod server;
mod stream;
mod sys;
mod tls;
mod upstream;

// Global allocator selection (compile-time feature; glibc by default).
// See rustle/Cargo.toml [features]: jemalloc | mimalloc. The benchmark
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match cli::parse(&args) {
        Err(msg) => {
            eprintln!("rustle: {msg}");
            2
        }
        Ok(command) => match dispatch(command) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("rustle: {e:#}");
                1
            }
        },
    };
    std::process::exit(code);
}

fn dispatch(command: cli::Command) -> anyhow::Result<i32> {
    use cli::Command::*;
    match command {
        Run(path) => run(path).map(|()| 0),
        Init { path, force } => cmd::init::run(path, force),
        Check(path) => cmd::check::run(path),
        Status(path) => cmd::status::run(path),
        Service(c) => cmd::service::run(c),
        Update { check, version } => cmd::update::run(check, version),
        Uninstall { purge } => cmd::uninstall::run(purge),
        Version => {
            println!("{}", cli::version_line());
            Ok(0)
        }
        Help(topic) => {
            println!("{}", cli::help(topic.as_deref()));
            Ok(0)
        }
    }
}

/// Runs the proxy until SIGTERM/SIGINT.
fn run(config_path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
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

    let config_path = cmd::resolve_config(config_path)?;
    let cfg = Arc::new(
        Config::load(Path::new(&config_path))
            .with_context(|| format!("loading config {}", config_path.display()))?,
    );
    if let Some((before, after)) = sys::raise_nofile_limit() {
        if after > before {
            tracing::info!(before, after, "raised the open-file limit");
        } else if before < 65536 {
            tracing::warn!(limit = before, "low open-file limit: a tunnel holds ~6 fds");
        }
    }

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
