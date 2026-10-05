//! `rustle status`: is the proxy answering, and what does its service say.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;

use super::resolve_config;
use crate::config::Config;

/// `GET /health` on the admin listener: true when it answers 200.
pub fn health(addr: SocketAddr) -> bool {
    let t = Duration::from_secs(2);
    let Ok(mut s) = TcpStream::connect_timeout(&addr, t) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(t));
    let _ = s.set_write_timeout(Some(t));
    if s.write_all(b"GET /health HTTP/1.1\r\nHost: rustle\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut head = [0u8; 32];
    let n = s.read(&mut head).unwrap_or(0);
    head[..n].starts_with(b"HTTP/1.1 200")
}

#[derive(clap::Args)]
pub struct Args {
    /// Config file (default: the usual lookup)
    pub config: Option<PathBuf>,
}

pub fn run(args: Args) -> anyhow::Result<i32> {
    let path = resolve_config(args.config)?;
    let cfg = Config::load(&path).with_context(|| format!("loading {}", path.display()))?;
    println!("config:  {}", path.display());
    println!("{}", super::service::state_summary());
    if cfg.server.admin_listen.is_empty() {
        println!("health:  admin listener disabled (server.admin_listen = \"\")");
        return Ok(0);
    }
    let addr: SocketAddr = cfg
        .server
        .admin_listen
        .parse()
        .with_context(|| format!("invalid admin_listen {:?}", cfg.server.admin_listen))?;
    if health(addr) {
        println!("health:  ok ({addr})");
        Ok(0)
    } else {
        println!("health:  not responding on {addr}");
        Ok(1)
    }
}
