//! Command line: `rustle [run] [CONFIG]` and the management subcommands.
//!
//! Backward compatible by construction: anything that is not a known
//! subcommand or flag is the config path (`rustle config.toml`), which is
//! how Docker entrypoints, the e2e harness and the bench start the proxy.

use std::path::PathBuf;

#[derive(Debug, PartialEq)]
pub enum Command {
    Run(Option<PathBuf>),
    Init {
        path: Option<PathBuf>,
        force: bool,
    },
    Check(Option<PathBuf>),
    Status(Option<PathBuf>),
    Service(ServiceCmd),
    Update {
        check: bool,
        version: Option<String>,
    },
    Uninstall {
        purge: bool,
    },
    Version,
    Help(Option<String>),
}

#[derive(Debug, PartialEq)]
pub struct ServiceCmd {
    pub action: ServiceAction,
    pub system: bool,
    pub config: Option<PathBuf>,
    pub dry_run: bool,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ServiceAction {
    Install,
    Uninstall,
    Enable,
    Disable,
    Start,
    Stop,
    Restart,
    Status,
    Logs,
}

const SERVICE_ACTIONS: &[(&str, ServiceAction)] = &[
    ("install", ServiceAction::Install),
    ("uninstall", ServiceAction::Uninstall),
    ("enable", ServiceAction::Enable),
    ("disable", ServiceAction::Disable),
    ("start", ServiceAction::Start),
    ("stop", ServiceAction::Stop),
    ("restart", ServiceAction::Restart),
    ("status", ServiceAction::Status),
    ("logs", ServiceAction::Logs),
];

/// Parses the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some(first) = args.first() else {
        return Ok(Command::Run(None));
    };
    let rest = &args[1..];
    match first.as_str() {
        "-V" | "--version" | "version" => Ok(Command::Version),
        "-h" | "--help" => Ok(Command::Help(None)),
        "help" => Ok(Command::Help(rest.first().cloned())),
        "run" => Ok(Command::Run(one_path(rest)?)),
        "check" => Ok(Command::Check(one_path(rest)?)),
        "status" => Ok(Command::Status(one_path(rest)?)),
        "init" => {
            let (mut path, mut force) = (None, false);
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--force" | "-f" => force = true,
                    "--path" => path = Some(value(&mut it, "--path")?.into()),
                    other => return Err(format!("init: unexpected argument `{other}`")),
                }
            }
            Ok(Command::Init { path, force })
        }
        "update" => {
            let (mut check, mut version) = (false, None);
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--check" => check = true,
                    "--version" => version = Some(value(&mut it, "--version")?),
                    other => return Err(format!("update: unexpected argument `{other}`")),
                }
            }
            Ok(Command::Update { check, version })
        }
        "uninstall" => {
            let mut purge = false;
            for a in rest {
                match a.as_str() {
                    "--purge" => purge = true,
                    other => return Err(format!("uninstall: unexpected argument `{other}`")),
                }
            }
            Ok(Command::Uninstall { purge })
        }
        "service" => {
            let Some(name) = rest.first() else {
                return Err(service_usage());
            };
            let Some(&(_, action)) = SERVICE_ACTIONS.iter().find(|(n, _)| n == name) else {
                return Err(format!(
                    "service: unknown action `{name}`\n{}",
                    service_usage()
                ));
            };
            let (mut system, mut config, mut dry_run) = (false, None, false);
            let mut it = rest[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--system" => system = true,
                    "--dry-run" => dry_run = true,
                    "--config" => config = Some(value(&mut it, "--config")?.into()),
                    other => return Err(format!("service {name}: unexpected argument `{other}`")),
                }
            }
            Ok(Command::Service(ServiceCmd {
                action,
                system,
                config,
                dry_run,
            }))
        }
        s if s.starts_with('-') => Err(format!("unknown option `{s}` (see `rustle --help`)")),
        // Anything else: the config path (historical form).
        path => {
            if !rest.is_empty() {
                return Err(format!("unexpected argument `{}`", rest[0]));
            }
            Ok(Command::Run(Some(path.into())))
        }
    }
}

fn one_path(rest: &[String]) -> Result<Option<PathBuf>, String> {
    match rest {
        [] => Ok(None),
        [p] if !p.starts_with('-') => Ok(Some(p.into())),
        [other, ..] => Err(format!("unexpected argument `{other}`")),
    }
}

fn value<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn service_usage() -> String {
    let names: Vec<&str> = SERVICE_ACTIONS.iter().map(|(n, _)| *n).collect();
    format!(
        "usage: rustle service <{}> [--system] [--config PATH] [--dry-run]",
        names.join("|")
    )
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const TARGET: &str = env!("RUSTLE_TARGET");

pub fn version_line() -> String {
    format!("rustle {VERSION} ({TARGET})")
}

pub fn help(topic: Option<&str>) -> String {
    match topic {
        Some("service") => format!(
            "{}\n\n\
             install     write the service definition (systemd unit / launchd agent)\n\
             uninstall   stop, disable and remove it\n\
             enable      install + start now and at boot/login\n\
             disable     stop and disable\n\
             start|stop|restart|status\n\
             logs        follow the service logs (journalctl / log file)\n\n\
             --system    Linux system-wide unit (root; dedicated `rustle` user,\n\
             \x20           config /etc/rustle/config.toml) instead of a user unit\n\
             --config    config file the service runs with\n\
             --dry-run   print what would be written/run, change nothing",
            service_usage()
        ),
        _ => format!(
            "{}\n\n\
             USAGE\n\
             \x20 rustle [run] [CONFIG]     run the proxy (default command)\n\
             \x20 rustle init               write a config with a fresh API key\n\
             \x20 rustle check [CONFIG]     validate a config without starting\n\
             \x20 rustle status [CONFIG]    health of the running proxy and its service\n\
             \x20 rustle service <action>   manage the systemd / launchd service\n\
             \x20 rustle update [--check]   update to the latest release\n\
             \x20 rustle uninstall [--purge]\n\
             \x20 rustle --version | --help\n\n\
             Without CONFIG the first existing file wins: ./config.toml,\n\
             ~/.config/rustle/config.toml, /etc/rustle/config.toml.\n\
             `rustle help service` lists the service actions.",
            version_line()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Command, String> {
        let v: Vec<String> = s.split_whitespace().map(String::from).collect();
        parse(&v)
    }

    #[test]
    fn config_path_stays_the_historical_form() {
        assert_eq!(p("").unwrap(), Command::Run(None));
        assert_eq!(
            p("/tmp/c.toml").unwrap(),
            Command::Run(Some("/tmp/c.toml".into()))
        );
        assert_eq!(
            p("run /tmp/c.toml").unwrap(),
            Command::Run(Some("/tmp/c.toml".into()))
        );
        assert!(p("a.toml b.toml").is_err());
    }

    #[test]
    fn flags_and_subcommands() {
        assert_eq!(p("--version").unwrap(), Command::Version);
        assert_eq!(p("-V").unwrap(), Command::Version);
        assert_eq!(p("-h").unwrap(), Command::Help(None));
        assert_eq!(p("check").unwrap(), Command::Check(None));
        assert!(p("--bogus").is_err());
        assert_eq!(
            p("init --path /x --force").unwrap(),
            Command::Init {
                path: Some("/x".into()),
                force: true
            }
        );
        assert_eq!(
            p("update --check --version v1.2.3").unwrap(),
            Command::Update {
                check: true,
                version: Some("v1.2.3".into())
            }
        );
        assert!(p("init --path").is_err());
    }

    #[test]
    fn service_actions() {
        assert_eq!(
            p("service enable --system --config /c.toml --dry-run").unwrap(),
            Command::Service(ServiceCmd {
                action: ServiceAction::Enable,
                system: true,
                config: Some("/c.toml".into()),
                dry_run: true
            })
        );
        assert!(p("service").is_err());
        assert!(p("service explode").is_err());
        assert!(p("service start --nope").is_err());
    }
}
