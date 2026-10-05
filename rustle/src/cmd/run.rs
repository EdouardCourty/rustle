//! `rustle [run] [CONFIG]`: the proxy itself, until SIGTERM/SIGINT.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;

use crate::config::Config;

#[derive(clap::Args)]
pub struct Args {
    /// Config file (default: ./config.toml, ~/.config/rustle/config.toml,
    /// /etc/rustle/config.toml — the first that exists)
    pub config: Option<PathBuf>,
}

/// Runs the proxy until SIGTERM/SIGINT.
pub fn run(args: Args) -> anyhow::Result<i32> {
    let config_path = args.config;
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

    let config_path = super::resolve_config(config_path)?;
    let cfg = Arc::new(
        Config::load(Path::new(&config_path))
            .with_context(|| format!("loading config {}", config_path.display()))?,
    );
    if let Some((before, after)) = crate::sys::raise_nofile_limit() {
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
    let srv = crate::server::start_with(cfg, Some(config_path.clone()))?;
    rt.block_on(async {
        loop {
            tokio::select! {
                _ = term.recv() => break,
                _ = int.recv() => break,
                // logrotate: rename the file, then `kill -HUP`.
                _ = hup.recv() => {
                    srv.reopen_logs();
                    match srv.reload() {
                        Ok(r) => tracing::info!(restart_required = ?r.restart_required, "config reloaded"),
                        Err(e) => tracing::warn!(error = %format!("{e:#}"), "config reload failed, keeping the running config"),
                    }
                }
            }
        }
    });
    tracing::info!("shutdown signal: draining active connections");
    srv.shutdown_and_join()?;
    Ok(0)
}
