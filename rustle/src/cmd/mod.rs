//! Management subcommands (everything except running the proxy).

pub mod check;
pub mod init;
pub mod service;
pub mod status;
pub mod uninstall;
pub mod update;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

/// `$HOME`, or an error telling what to set.
pub fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("$HOME is not set")
}

/// `$XDG_CONFIG_HOME/rustle` or `~/.config/rustle`.
pub fn user_config_dir() -> anyhow::Result<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => Ok(PathBuf::from(x).join("rustle")),
        None => Ok(home()?.join(".config").join("rustle")),
    }
}

pub fn user_config_path() -> anyhow::Result<PathBuf> {
    Ok(user_config_dir()?.join("config.toml"))
}

pub const SYSTEM_CONFIG: &str = "/etc/rustle/config.toml";

/// Where a config is looked for when none is given, in order.
pub fn config_candidates() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("config.toml")];
    if let Ok(p) = user_config_path() {
        v.push(p);
    }
    v.push(PathBuf::from(SYSTEM_CONFIG));
    v
}

/// The given config, or the first existing candidate.
pub fn resolve_config(given: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(p) = given {
        return Ok(p);
    }
    let cands = config_candidates();
    if let Some(p) = cands.iter().find(|p| p.is_file()) {
        return Ok(p.clone());
    }
    let tried: Vec<String> = cands.iter().map(|p| format!("  {}", p.display())).collect();
    bail!(
        "no config file found, tried:\n{}\nrun `rustle init` to create one, or pass a path",
        tried.join("\n")
    )
}

/// Runs `program args…`, inheriting stdio; returns its exit code.
pub fn run_inherit(program: &str, args: &[&str]) -> anyhow::Result<i32> {
    let st = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("running `{program}` (is it installed?)"))?;
    Ok(st.code().unwrap_or(1))
}

/// `program args…` captured: (exit code, stdout).
pub fn run_capture(program: &str, args: &[&str]) -> anyhow::Result<(i32, String)> {
    let o = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running `{program}` (is it installed?)"))?;
    Ok((
        o.status.code().unwrap_or(1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
    ))
}

/// Quotes a path for a unit/plist `ExecStart` only when needed.
pub fn quote_arg(p: &Path) -> String {
    let s = p.display().to_string();
    if s.contains([' ', '\t', '"', '\'']) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote_arg(Path::new("/usr/bin/rustle")), "/usr/bin/rustle");
        assert_eq!(quote_arg(Path::new("/my dir/rustle")), "\"/my dir/rustle\"");
    }
}
