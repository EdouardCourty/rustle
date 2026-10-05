//! `rustle check`: validate a config (parse, policies, TLS material)
//! without opening a single listener.

use std::path::PathBuf;

use anyhow::Context;

use super::resolve_config;
use crate::config::Config;
use crate::policy::{ClientPolicy, DestPolicy};

pub fn run(path: Option<PathBuf>) -> anyhow::Result<i32> {
    let path = resolve_config(path)?;
    let cfg = Config::load(&path).with_context(|| format!("loading {}", path.display()))?;
    DestPolicy::from_config(&cfg).context("[filter]")?;
    ClientPolicy::from_config(&cfg).context("[auth] allow_clients")?;
    if !cfg.tls.listen.is_empty() {
        crate::tls::acceptor(&cfg.tls, false).context("[tls] certificate / key")?;
    }
    println!("ok: {}", path.display());
    println!("  listen   {}", cfg.server.listen);
    if !cfg.tls.listen.is_empty() {
        println!("  tls      {}", cfg.tls.listen);
    }
    if !cfg.socks5.listen.is_empty() {
        println!("  socks5   {}", cfg.socks5.listen);
    }
    if cfg.auth.api_keys.is_empty() {
        println!("  warning  no api_keys: this is an open relay");
    } else {
        println!("  api keys {}", cfg.auth.api_keys.len());
    }
    Ok(0)
}
