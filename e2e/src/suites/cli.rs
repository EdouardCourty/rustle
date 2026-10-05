//! The command line: version/help, argument errors, `init` → `check` →
//! run, `status` against a live proxy, `service … --dry-run`.

use anyhow::{ensure, Context};

use crate::harness::proxy::binary;
use crate::harness::{Config, Proxy};
use crate::runner::{scenario, Scenario};

/// Runs the binary with `args`: (exit code, stdout, stderr).
fn rustle(args: &[&str], envs: &[(&str, &str)]) -> anyhow::Result<(i32, String, String)> {
    let o = std::process::Command::new(binary())
        .args(args)
        .envs(envs.iter().copied())
        .stdin(std::process::Stdio::null())
        .output()
        .context("spawning the binary")?;
    Ok((
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    ))
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("rustle-e2e-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn scenarios() -> Vec<Scenario> {
    vec![
        scenario("cli/version-and-help", |_env| async move {
            for flag in ["--version", "-V", "version"] {
                let (code, out, _) = rustle(&[flag], &[])?;
                ensure!(
                    code == 0 && out.starts_with("rustle "),
                    "{flag}: {code} {out}"
                );
            }
            let (code, out, _) = rustle(&["--help"], &[])?;
            ensure!(
                code == 0 && out.contains("USAGE") && out.contains("service"),
                "{out}"
            );
            let (code, out, _) = rustle(&["help", "service"], &[])?;
            ensure!(code == 0 && out.contains("--dry-run"), "{out}");
            Ok(())
        })
        .quick(),
        scenario("cli/argument-errors", |_env| async move {
            for args in [
                &["--bogus"][..],
                &["service"],
                &["service", "explode"],
                &["init", "--path"],
            ] {
                let (code, _, err) = rustle(args, &[])?;
                ensure!(code == 2, "{args:?}: exit {code}, stderr {err}");
                ensure!(err.starts_with("rustle: "), "{args:?}: {err}");
            }
            Ok(())
        })
        .quick(),
        scenario("cli/init-check-and-run", |env| async move {
            let dir = tmp_dir("init");
            let path = dir.join("sub").join("config.toml");
            let p = path.display().to_string();
            let (code, out, err) = rustle(&["init", "--path", &p], &[])?;
            ensure!(code == 0, "init: {code} {err}");
            let key = out
                .lines()
                .find_map(|l| l.strip_prefix("API key: "))
                .context("no API key printed")?
                .to_string();
            ensure!(key.len() == 48, "key {key}");
            // Never overwrites silently.
            let (code, _, err) = rustle(&["init", "--path", &p], &[])?;
            ensure!(code == 1 && err.contains("already exists"), "{code} {err}");
            // The generated config is valid as is…
            let (code, out, err) = rustle(&["check", &p], &[])?;
            ensure!(
                code == 0 && out.starts_with("ok:"),
                "check: {code} {out} {err}"
            );
            // …and runs with the generated key (ports moved to free ones).
            let text = std::fs::read_to_string(&path)?
                .replace("\"0.0.0.0:8080\"", "\"127.0.0.1:0\"")
                .replace("\"127.0.0.1:8181\"", "\"127.0.0.1:0\"");
            let px = Proxy::start_text(&dir, &text).await?;
            let mut c = crate::clients::http1::Client::plain(px.addr).await?;
            let req = crate::clients::http1::request(
                "GET",
                &env.origin.url("/data/1"),
                Some(&crate::clients::basic("u", &key)),
                "",
                b"",
            );
            // The example blocks private destinations (403 = authenticated).
            c.roundtrip(&req, false).await?.expect(403)?;
            drop(c);
            let _ = std::fs::remove_dir_all(&dir);
            px.finish().await
        }),
        scenario("cli/check-rejects-bad-config", |_env| async move {
            let dir = tmp_dir("check");
            let p = dir.join("bad.toml");
            std::fs::write(&p, "[server]\nlisten = \"127.0.0.1:0\"\nlisen = 1\n")?;
            let (code, _, err) = rustle(&["check", &p.display().to_string()], &[])?;
            ensure!(code == 1 && err.contains("unknown field"), "{code} {err}");
            let (code, _, err) = rustle(
                &["check"],
                &[
                    ("HOME", &dir.display().to_string()),
                    ("XDG_CONFIG_HOME", ""),
                ],
            )?;
            // No config anywhere near: lists what it tried.
            ensure!(
                code == 1 && err.contains("no config file found"),
                "{code} {err}"
            );
            let _ = std::fs::remove_dir_all(&dir);
            Ok(())
        })
        .quick(),
        scenario("cli/status-reports-health", |_env| async move {
            let admin = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?;
            let cfg = Config::new().set("server", "admin_listen", &format!("\"{admin}\""));
            let px = Proxy::start_text(&cfg.dir, &cfg.render()).await?;
            let path = px.dir.join("config.toml").display().to_string();
            let (code, out, err) = rustle(&["status", &path], &[])?;
            ensure!(
                code == 0 && out.contains("health:  ok"),
                "{code} {out} {err}"
            );
            // The harness removes its directory on finish: keep a copy.
            let keep = tmp_dir("status");
            let kept = keep.join("config.toml");
            std::fs::copy(&path, &kept)?;
            px.finish().await?;
            // Stopped: the same command now fails.
            let (code, out, _) = rustle(&["status", &kept.display().to_string()], &[])?;
            ensure!(
                code == 1 && out.contains("not responding"),
                "{code} {out:?}"
            );
            let _ = std::fs::remove_dir_all(keep);
            Ok(())
        }),
        scenario(
            "cli/service-dry-run-renders-the-definition",
            |_env| async move {
                let dir = tmp_dir("svc");
                let cfg = dir.join("c.toml");
                std::fs::write(&cfg, "[server]\nlisten = \"127.0.0.1:0\"\n")?;
                let cfg_s = cfg.display().to_string();
                let envs = [("HOME", dir.to_str().unwrap()), ("XDG_CONFIG_HOME", "")];
                let (code, out, err) = rustle(
                    &["service", "enable", "--config", &cfg_s, "--dry-run"],
                    &envs,
                )?;
                ensure!(code == 0, "{code} {err}");
                ensure!(
                    out.contains(&format!("run {cfg_s}"))
                        || out.contains(&format!("<string>{cfg_s}</string>")),
                    "{out}"
                );
                if cfg!(target_os = "linux") {
                    ensure!(
                        out.contains("LimitNOFILE=1048576") && out.contains("systemctl --user"),
                        "{out}"
                    );
                    let (code, out, _) = rustle(
                        &[
                            "service",
                            "install",
                            "--system",
                            "--config",
                            &cfg_s,
                            "--dry-run",
                        ],
                        &envs,
                    )?;
                    ensure!(
                        code == 0 && out.contains("User=rustle") && out.contains("useradd"),
                        "{out}"
                    );
                } else {
                    ensure!(out.contains("launchctl bootstrap"), "{out}");
                }
                // Dry run changes nothing.
                ensure!(
                    !dir.join(".config").exists() && !dir.join("Library").exists(),
                    "dry-run wrote files"
                );
                let _ = std::fs::remove_dir_all(&dir);
                Ok(())
            },
        ),
    ]
}
