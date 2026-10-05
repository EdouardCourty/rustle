//! `rustle service …`: run the proxy as a managed service.
//!
//! Linux = systemd (a user unit by default, `--system` for a system-wide
//! one); macOS = a launchd agent. Everything shells out to `systemctl` /
//! `launchctl` / `journalctl`; the unit and plist are rendered by pure
//! functions (unit-tested, and printed by `--dry-run`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

use super::{quote_arg, resolve_config, run_capture, run_inherit, user_config_dir, SYSTEM_CONFIG};

#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum Action {
    /// Write the service definition (systemd unit / launchd agent)
    Install,
    /// Stop, disable and remove it
    Uninstall,
    /// Install, start now and at boot / login
    Enable,
    /// Stop and disable
    Disable,
    Start,
    Stop,
    Restart,
    Status,
    /// Follow the service logs (journalctl / log file)
    Logs,
}

#[derive(clap::Args)]
pub struct Args {
    #[arg(value_enum)]
    pub action: Action,
    /// Linux: system-wide unit (needs root; dedicated `rustle` user, config
    /// /etc/rustle/config.toml) instead of a user unit
    #[arg(long)]
    pub system: bool,
    /// Config file the service runs with
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Print what would be written / run; change nothing
    #[arg(long)]
    pub dry_run: bool,
}

const NAME: &str = "rustle";
const LABEL: &str = "com.edouardcourty.rustle";
const SYSTEM_USER: &str = "rustle";

pub fn render_systemd_unit(exe: &Path, config: &Path, system: bool) -> String {
    let (hardening, wanted_by) = if system {
        (
            format!(
                "User={SYSTEM_USER}\n\
                 LogsDirectory=rustle\n\
                 AmbientCapabilities=CAP_NET_BIND_SERVICE\n\
                 CapabilityBoundingSet=CAP_NET_BIND_SERVICE\n\
                 NoNewPrivileges=yes\n\
                 PrivateTmp=yes\n\
                 ProtectSystem=strict\n\
                 ProtectHome=yes\n"
            ),
            "multi-user.target",
        )
    } else {
        (
            "NoNewPrivileges=yes\nPrivateTmp=yes\n".to_string(),
            "default.target",
        )
    };
    format!(
        "[Unit]\n\
         Description=rustle forward proxy\n\
         Documentation=https://github.com/EdouardCourty/rustle\n\
         After=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} run {config}\n\
         ExecReload=/bin/kill -HUP $MAINPID\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         KillSignal=SIGTERM\n\
         TimeoutStopSec=10\n\
         LimitNOFILE=1048576\n\
         {hardening}\n\
         [Install]\n\
         WantedBy={wanted_by}\n",
        exe = quote_arg(exe),
        config = quote_arg(config),
    )
}

pub fn render_launchd_plist(exe: &Path, config: &Path, log: &Path) -> String {
    let esc = |p: &Path| {
        p.display()
            .to_string()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key><string>{LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         \x20   <string>{exe}</string>\n\
         \x20   <string>run</string>\n\
         \x20   <string>{config}</string>\n\
         \x20 </array>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><true/>\n\
         \x20 <key>ProcessType</key><string>Interactive</string>\n\
         \x20 <key>SoftResourceLimits</key><dict><key>NumberOfFiles</key><integer>65536</integer></dict>\n\
         \x20 <key>StandardOutPath</key><string>{log}</string>\n\
         \x20 <key>StandardErrorPath</key><string>{log}</string>\n\
         </dict>\n\
         </plist>\n",
        exe = esc(exe),
        config = esc(config),
        log = esc(log),
    )
}

struct Ctx {
    system: bool,
    dry: bool,
    config: Option<PathBuf>,
}

impl Ctx {
    /// `program args` (printed only, under `--dry-run`); a failure aborts.
    fn must(&self, program: &str, args: &[&str]) -> anyhow::Result<()> {
        if self.dry {
            println!("+ {program} {}", args.join(" "));
            return Ok(());
        }
        let code = run_inherit(program, args)?;
        if code != 0 {
            bail!("`{program} {}` failed (exit {code})", args.join(" "));
        }
        Ok(())
    }

    /// Same, failure tolerated (cleanup paths).
    fn try_run(&self, program: &str, args: &[&str]) {
        if self.dry {
            println!("+ {program} {}", args.join(" "));
        } else {
            // Quiet: these are "already gone" cases (bootout of a service
            // that is not loaded, disable of a missing unit).
            let _ = std::process::Command::new(program)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    fn write(&self, path: &Path, text: &str) -> anyhow::Result<()> {
        if self.dry {
            println!("# {}\n{text}", path.display());
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
        Ok(())
    }
}

fn is_macos() -> bool {
    cfg!(target_os = "macos")
}

fn exe_path() -> anyhow::Result<PathBuf> {
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("locating the rustle executable")
}

fn absolute(p: PathBuf) -> anyhow::Result<PathBuf> {
    if p.is_absolute() {
        Ok(p)
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

/// The config the service runs with: `--config`, else the system path
/// (`--system`) or the first existing candidate. Must exist outside
/// `--dry-run`.
fn config_for(ctx: &Ctx) -> anyhow::Result<PathBuf> {
    let p = match (&ctx.config, ctx.system) {
        (Some(p), _) => p.clone(),
        (None, true) => PathBuf::from(SYSTEM_CONFIG),
        (None, false) => match resolve_config(None) {
            Ok(p) => p,
            Err(e) if ctx.dry => {
                eprintln!("note: {e:#}");
                user_config_dir()?.join("config.toml")
            }
            Err(e) => return Err(e),
        },
    };
    let p = absolute(p)?;
    if !ctx.dry && !p.is_file() {
        bail!(
            "config {} not found: create it with `rustle init --path {}`",
            p.display(),
            p.display()
        );
    }
    Ok(p)
}

fn systemd_unit_path(system: bool) -> anyhow::Result<PathBuf> {
    if system {
        return Ok(PathBuf::from(format!("/etc/systemd/system/{NAME}.service")));
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => super::home()?.join(".config"),
    };
    Ok(base.join("systemd/user").join(format!("{NAME}.service")))
}

fn plist_path() -> anyhow::Result<PathBuf> {
    Ok(super::home()?
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn log_path() -> anyhow::Result<PathBuf> {
    Ok(super::home()?.join("Library/Logs/rustle.log"))
}

/// `gui/<uid>/<label>`: the launchd domain target of the user agent.
fn launchd_target() -> String {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    format!("gui/{uid}/{LABEL}")
}

fn launchd_domain() -> String {
    // SAFETY: as above.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn systemctl_args<'a>(system: bool, rest: &[&'a str]) -> Vec<&'a str> {
    let mut v = Vec::new();
    if !system {
        v.push("--user");
    }
    v.extend_from_slice(rest);
    v
}

fn require_root(ctx: &Ctx) -> anyhow::Result<()> {
    // SAFETY: geteuid has no preconditions.
    if ctx.system && !ctx.dry && unsafe { libc::geteuid() } != 0 {
        bail!("--system needs root: re-run with sudo");
    }
    Ok(())
}

fn install(ctx: &Ctx) -> anyhow::Result<()> {
    let exe = exe_path()?;
    if is_macos() {
        if ctx.system {
            bail!("--system is not supported on macOS (launchd agent only)");
        }
        let config = config_for(ctx)?;
        let log = log_path()?;
        if !ctx.dry {
            if let Some(d) = log.parent() {
                std::fs::create_dir_all(d)?;
            }
        }
        return ctx.write(&plist_path()?, &render_launchd_plist(&exe, &config, &log));
    }
    require_root(ctx)?;
    let config = config_for(ctx)?;
    if ctx.system {
        // Dedicated unprivileged user; the config (it holds API keys) is
        // readable by it and by root only.
        let exists = run_capture("id", &["-u", SYSTEM_USER]).is_ok_and(|(c, _)| c == 0);
        if !exists || ctx.dry {
            ctx.try_run(
                "useradd",
                &[
                    "--system",
                    "--no-create-home",
                    "--shell",
                    "/usr/sbin/nologin",
                    SYSTEM_USER,
                ],
            );
        }
        let cfg = config.display().to_string();
        ctx.must("chown", &[&format!("root:{SYSTEM_USER}"), &cfg])?;
        ctx.must("chmod", &["640", &cfg])?;
    }
    ctx.write(
        &systemd_unit_path(ctx.system)?,
        &render_systemd_unit(&exe, &config, ctx.system),
    )?;
    ctx.must("systemctl", &systemctl_args(ctx.system, &["daemon-reload"]))
}

fn enable(ctx: &Ctx) -> anyhow::Result<()> {
    install(ctx)?;
    if is_macos() {
        ctx.try_run("launchctl", &["bootout", &launchd_target()]);
        let plist = plist_path()?.display().to_string();
        ctx.must("launchctl", &["bootstrap", &launchd_domain(), &plist])?;
        ctx.must("launchctl", &["enable", &launchd_target()])?;
        println!("rustle is running and starts at login (log: ~/Library/Logs/rustle.log)");
    } else {
        ctx.must(
            "systemctl",
            &systemctl_args(ctx.system, &["enable", "--now", NAME]),
        )?;
        if ctx.system {
            println!("rustle is running and starts at boot");
        } else {
            println!(
                "rustle is running. To keep it running after you log out:\n  \
                 sudo loginctl enable-linger $USER"
            );
        }
    }
    Ok(())
}

fn disable(ctx: &Ctx) -> anyhow::Result<()> {
    if is_macos() {
        ctx.try_run("launchctl", &["bootout", &launchd_target()]);
        ctx.try_run("launchctl", &["disable", &launchd_target()]);
    } else {
        require_root(ctx)?;
        ctx.try_run(
            "systemctl",
            &systemctl_args(ctx.system, &["disable", "--now", NAME]),
        );
    }
    Ok(())
}

fn uninstall(ctx: &Ctx) -> anyhow::Result<()> {
    let path = if is_macos() {
        plist_path()?
    } else {
        systemd_unit_path(ctx.system)?
    };
    if !ctx.dry && !path.exists() {
        println!("no service installed ({} not found)", path.display());
        return Ok(());
    }
    disable(ctx)?;
    if ctx.dry {
        println!("+ rm {}", path.display());
    } else if path.exists() {
        std::fs::remove_file(&path)?;
        println!("removed {}", path.display());
    }
    if !is_macos() {
        ctx.try_run("systemctl", &systemctl_args(ctx.system, &["daemon-reload"]));
    }
    Ok(())
}

/// Runs a direct control verb; returns the tool's exit code.
fn control(ctx: &Ctx, action: Action) -> anyhow::Result<i32> {
    let verb = match action {
        Action::Start => "start",
        Action::Stop => "stop",
        Action::Restart => "restart",
        Action::Status => "status",
        _ => unreachable!(),
    };
    if is_macos() {
        let target = launchd_target();
        let loaded = run_capture("launchctl", &["print", &target]).is_ok_and(|(c, _)| c == 0);
        return match action {
            Action::Start => {
                if !loaded {
                    let plist = plist_path()?.display().to_string();
                    ctx.must("launchctl", &["bootstrap", &launchd_domain(), &plist])?;
                }
                ctx.must("launchctl", &["kickstart", &target])?;
                Ok(0)
            }
            Action::Stop => {
                ctx.must("launchctl", &["bootout", &target])?;
                Ok(0)
            }
            Action::Restart => {
                ctx.must("launchctl", &["kickstart", "-k", &target])?;
                Ok(0)
            }
            _ => {
                let state = launchd_state();
                println!("rustle: {state}");
                Ok(if state == "running" { 0 } else { 3 })
            }
        };
    }
    if verb == "status" {
        return run_inherit(
            "systemctl",
            &systemctl_args(ctx.system, &["status", "--no-pager", NAME]),
        );
    }
    require_root(ctx)?;
    ctx.must("systemctl", &systemctl_args(ctx.system, &[verb, NAME]))?;
    Ok(0)
}

fn logs(ctx: &Ctx) -> anyhow::Result<i32> {
    if is_macos() {
        let log = log_path()?.display().to_string();
        return run_inherit("tail", &["-n", "100", "-f", &log]);
    }
    let mut a = Vec::new();
    if !ctx.system {
        a.push("--user");
    }
    a.extend(["-u", NAME, "-n", "100", "-f"]);
    run_inherit("journalctl", &a)
}

pub fn run(cmd: Args) -> anyhow::Result<i32> {
    let ctx = Ctx {
        system: cmd.system,
        dry: cmd.dry_run,
        config: cmd.config,
    };
    match cmd.action {
        Action::Install => install(&ctx)?,
        Action::Uninstall => uninstall(&ctx)?,
        Action::Enable => enable(&ctx)?,
        Action::Disable => disable(&ctx)?,
        Action::Logs => return logs(&ctx),
        a @ (Action::Start | Action::Stop | Action::Restart | Action::Status) => {
            return control(&ctx, a)
        }
    }
    Ok(0)
}

fn launchd_state() -> String {
    match run_capture("launchctl", &["print", &launchd_target()]) {
        Ok((0, out)) => out
            .lines()
            .find_map(|l| l.trim().strip_prefix("state = "))
            .unwrap_or("loaded")
            .to_string(),
        _ => "not running".to_string(),
    }
}

/// One-line state of the installed service, for `rustle status`.
pub fn state_summary() -> String {
    if is_macos() {
        return match plist_path() {
            Ok(p) if p.exists() => format!("launchd agent: {}", launchd_state()),
            _ => "service: not installed".to_string(),
        };
    }
    for system in [false, true] {
        let Ok(p) = systemd_unit_path(system) else {
            continue;
        };
        if p.exists() {
            let args = systemctl_args(system, &["is-active", NAME]);
            let state = run_capture("systemctl", &args)
                .map(|(_, o)| o.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            let scope = if system { "system" } else { "user" };
            return format!("systemd {scope} unit: {state}");
        }
    }
    "service: not installed".to_string()
}

/// Restarts the installed service after an update (best effort). Returns
/// what was restarted, if anything.
pub fn restart_if_installed() -> Option<String> {
    if is_macos() {
        if plist_path().ok()?.exists() {
            let _ = run_capture("launchctl", &["kickstart", "-k", &launchd_target()]);
            return Some("launchd agent".to_string());
        }
        return None;
    }
    for system in [false, true] {
        let p = systemd_unit_path(system).ok()?;
        if p.exists() {
            let args = systemctl_args(system, &["restart", NAME]);
            return match run_capture("systemctl", &args) {
                Ok((0, _)) => Some(format!(
                    "systemd {} unit",
                    if system { "system" } else { "user" }
                )),
                _ => None,
            };
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_unit() {
        let u = render_systemd_unit(
            Path::new("/home/u/.local/bin/rustle"),
            Path::new("/home/u/c.toml"),
            false,
        );
        assert!(u.contains("ExecStart=/home/u/.local/bin/rustle run /home/u/c.toml\n"));
        assert!(u.contains("LimitNOFILE=1048576"));
        assert!(u.contains("ExecReload=/bin/kill -HUP $MAINPID"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(!u.contains("User="));
    }

    #[test]
    fn system_unit() {
        let u = render_systemd_unit(
            Path::new("/usr/local/bin/rustle"),
            Path::new("/etc/rustle/config.toml"),
            true,
        );
        assert!(u.contains("User=rustle"));
        assert!(u.contains("ProtectSystem=strict"));
        assert!(u.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn plist_is_escaped_and_complete() {
        let p = render_launchd_plist(
            Path::new("/a & b/rustle"),
            Path::new("/c.toml"),
            Path::new("/l.log"),
        );
        assert!(p.contains("<string>/a &amp; b/rustle</string>"));
        assert!(p.contains("<string>run</string>"));
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        assert!(p.contains(LABEL));
    }
}
