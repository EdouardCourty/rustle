//! Command line. `rustle [CONFIG]` and `rustle run [CONFIG]` run the proxy
//! (a bare path is the historical form, kept for Docker entrypoints, the
//! e2e harness and the bench); the other subcommands live in `cmd/`, one
//! file each with its own clap `Args`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::cmd;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const TARGET: &str = env!("RUSTLE_TARGET");
const LONG_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("RUSTLE_TARGET"), ")");

#[derive(Parser)]
#[command(
    name = "rustle",
    version = LONG_VERSION,
    about = "High-performance forward proxy: HTTP/1.1, HTTPS, HTTP/2, SOCKS5",
    // `rustle config.toml` (positional) vs `rustle init` (subcommand).
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Config file to run with (same as `rustle run CONFIG`)
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the proxy in the foreground (default command)
    Run(cmd::run::Args),
    /// Write a config with a freshly generated API key
    Init(cmd::init::Args),
    /// Validate a config without starting the proxy
    Check(cmd::check::Args),
    /// Health of the running proxy and of its service
    Status(cmd::status::Args),
    /// Manage the systemd / launchd service
    Service(cmd::service::Args),
    /// Update to the latest release (checksum verified)
    Update(cmd::update::Args),
    /// Remove the service and the binary
    Uninstall(cmd::uninstall::Args),
}

impl Cli {
    /// Runs the chosen command; the value is the process exit code.
    pub fn dispatch(self) -> anyhow::Result<i32> {
        match self.command {
            None => cmd::run::run(cmd::run::Args {
                config: self.config,
            }),
            Some(Command::Run(a)) => cmd::run::run(a),
            Some(Command::Init(a)) => cmd::init::run(a),
            Some(Command::Check(a)) => cmd::check::run(a),
            Some(Command::Status(a)) => cmd::status::run(a),
            Some(Command::Service(a)) => cmd::service::run(a),
            Some(Command::Update(a)) => cmd::update::run(a),
            Some(Command::Uninstall(a)) => cmd::uninstall::run(a),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn p(s: &str) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("rustle").chain(s.split_whitespace()))
    }

    #[test]
    fn definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn config_path_stays_the_historical_form() {
        let c = p("").unwrap();
        assert!(c.command.is_none() && c.config.is_none());
        let c = p("/tmp/c.toml").unwrap();
        assert_eq!(c.config, Some("/tmp/c.toml".into()));
        assert!(c.command.is_none());
        assert!(matches!(
            p("run /tmp/c.toml").unwrap().command,
            Some(Command::Run(_))
        ));
        assert!(p("a.toml b.toml").is_err());
    }

    #[test]
    fn subcommands_and_errors() {
        assert!(matches!(
            p("init --path /x --force").unwrap().command,
            Some(Command::Init(_))
        ));
        assert!(matches!(
            p("update --check --version v1.2.3").unwrap().command,
            Some(Command::Update(_))
        ));
        assert!(matches!(
            p("service enable --system --config /c.toml --dry-run")
                .unwrap()
                .command,
            Some(Command::Service(_))
        ));
        for bad in [
            "--bogus",
            "service",
            "service explode",
            "service start --nope",
            "init --path",
        ] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }
}
