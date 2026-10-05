//! `rustle reload`: ask the running proxy, over its control socket, to
//! re-read its config file.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;

use super::resolve_config;
use crate::config::Config;

#[derive(clap::Args)]
pub struct Args {
    /// Config file used to find the control socket (default: the usual
    /// lookup). The proxy re-reads the file it was started with.
    pub config: Option<PathBuf>,
}

pub fn run(args: Args) -> anyhow::Result<i32> {
    let path = resolve_config(args.config)?;
    let cfg = Config::load(&path).with_context(|| format!("loading {}", path.display()))?;
    let Some(sock) = cfg.server.control_socket_path() else {
        eprintln!(
            "control socket disabled (server.control_socket = \"\"): send SIGHUP to the process instead (kill -HUP <pid>)"
        );
        return Ok(1);
    };
    let t = Duration::from_secs(10);
    let mut s = UnixStream::connect(&sock).with_context(|| {
        format!(
            "connecting to {}: is rustle running (as this user)?",
            sock.display()
        )
    })?;
    s.set_read_timeout(Some(t))?;
    s.set_write_timeout(Some(t))?;
    s.write_all(b"reload\n")?;
    let mut reply = String::new();
    s.read_to_string(&mut reply)?;
    let reply = reply.trim();
    match reply.split_once(' ') {
        Some(("ok", msg)) => {
            println!("{msg}");
            Ok(0)
        }
        Some(("error", msg)) => {
            eprintln!("{msg}");
            Ok(1)
        }
        _ => anyhow::bail!("unexpected answer from {}: {reply:?}", sock.display()),
    }
}
