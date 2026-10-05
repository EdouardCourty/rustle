//! Config reload: `rustle reload` (control socket) and SIGHUP apply keys,
//! limits and the TLS certificate to new connections; a broken file or a
//! restart-only setting never takes the proxy down.

use std::path::Path;
use std::time::Duration;

use anyhow::{ensure, Context};

use crate::clients::basic;
use crate::clients::http1::{request, Client};
use crate::harness::proxy::{binary, KEY, KEY2};
use crate::harness::{Config, Proxy};
use crate::runner::{scenario, Scenario};

/// Runs `rustle reload <dir>/config.toml`: (exit code, stdout, stderr).
fn reload_cmd(dir: &Path) -> anyhow::Result<(i32, String, String)> {
    let o = std::process::Command::new(binary())
        .arg("reload")
        .arg(dir.join("config.toml"))
        .output()
        .context("running rustle reload")?;
    Ok((
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    ))
}

/// Status of a GET through the proxy with `key` (fresh connection).
async fn status_with(p: &Proxy, url: &str, key: &str) -> anyhow::Result<u16> {
    let mut c = Client::plain(p.addr).await?;
    let req = request("GET", url, Some(&basic("u", key)), "", b"");
    Ok(c.roundtrip(&req, false).await?.status)
}

/// Rewrites the proxy's config file.
fn write_config(cfg: &Config) -> anyhow::Result<()> {
    std::fs::write(cfg.dir.join("config.toml"), cfg.render())?;
    Ok(())
}

fn only_key(key: &str) -> String {
    format!("[\"{key}\"]")
}

pub fn scenarios() -> Vec<Scenario> {
    vec![
        scenario("reload/command-swaps-keys", |env| async move {
            let cfg = Config::new();
            let p = Proxy::start_text(&cfg.dir, &cfg.render()).await?;
            let url = env.origin.url("/data/1");
            ensure!(status_with(&p, &url, KEY).await? == 200);
            ensure!(status_with(&p, &url, KEY2).await? == 200);

            let cfg = cfg.set("auth", "api_keys", &only_key(KEY2));
            write_config(&cfg)?;
            let (code, out, err) = reload_cmd(&cfg.dir)?;
            ensure!(
                code == 0 && out.trim() == "reloaded",
                "{code} {out:?} {err:?}"
            );
            ensure!(
                status_with(&p, &url, KEY).await? == 407,
                "revoked key still accepted"
            );
            ensure!(status_with(&p, &url, KEY2).await? == 200);
            ensure!(p.stderr().contains("config reloaded"));
            p.finish().await
        })
        .quick(),
        scenario("reload/sighup-swaps-keys", |env| async move {
            let cfg = Config::new();
            let p = Proxy::start_text(&cfg.dir, &cfg.render()).await?;
            let url = env.origin.url("/data/1");
            let cfg = cfg.set("auth", "api_keys", &only_key(KEY2));
            write_config(&cfg)?;
            p.signal(libc::SIGHUP);
            p.wait_for("config reloaded", Duration::from_secs(5), |p| {
                p.stderr().contains("config reloaded")
            })
            .await?;
            ensure!(status_with(&p, &url, KEY).await? == 407);
            ensure!(status_with(&p, &url, KEY2).await? == 200);
            p.finish().await
        }),
        scenario("reload/connections-keep-their-config", |env| async move {
            // A keep-alive connection accepted before the reload keeps the
            // config it started with; new connections get the new one.
            let cfg = Config::new();
            let p = Proxy::start_text(&cfg.dir, &cfg.render()).await?;
            let url = env.origin.url("/data/1");
            let mut old = Client::plain(p.addr).await?;
            let req = request("GET", &url, Some(&basic("u", KEY)), "", b"");
            old.roundtrip(&req, false).await?.expect(200)?;

            let cfg = cfg.set("auth", "api_keys", &only_key(KEY2));
            write_config(&cfg)?;
            ensure!(reload_cmd(&cfg.dir)?.0 == 0);
            old.roundtrip(&req, false).await?.expect(200)?;
            ensure!(status_with(&p, &url, KEY).await? == 407);
            drop(old);
            p.finish().await
        }),
        scenario("reload/invalid-config-keeps-running", |env| async move {
            let cfg = Config::new();
            let p = Proxy::start_text(&cfg.dir, &cfg.render()).await?;
            let url = env.origin.url("/data/1");
            let bad_cidr = Config::new().set("filter", "deny", "[\"10.0.0.0/99\"]");
            for bad in [
                "this is = = not toml".to_string(),
                "[server]\nlisen = 1\n".to_string(),
                // Parses, so only the proxy can refuse it.
                bad_cidr.render().replace(
                    &bad_cidr.dir.display().to_string(),
                    &cfg.dir.display().to_string(),
                ),
            ] {
                std::fs::write(cfg.dir.join("config.toml"), bad)?;
                let (code, out, err) = reload_cmd(&cfg.dir)?;
                // The command itself loads the file first: it fails or the
                // proxy refuses it; either way the exit code is not 0.
                ensure!(code != 0, "accepted a broken config: {out:?} {err:?}");
                ensure!(
                    status_with(&p, &url, KEY).await? == 200,
                    "proxy affected by a bad reload"
                );
            }
            // The proxy itself refuses what the CLI let through (bad CIDR).
            ensure!(
                p.stderr().contains("config reload failed"),
                "{}",
                p.stderr()
            );
            p.finish().await
        }),
        scenario(
            "reload/restart-only-settings-are-reported",
            |env| async move {
                let cfg = Config::new();
                let p = Proxy::start_text(&cfg.dir, &cfg.render()).await?;
                let cfg =
                    cfg.set("server", "workers", "2")
                        .set("auth", "api_keys", &only_key(KEY2));
                write_config(&cfg)?;
                let (code, out, err) = reload_cmd(&cfg.dir)?;
                ensure!(code == 0, "{err}");
                ensure!(
                    out.contains("restart required for: server.workers"),
                    "{out:?}"
                );
                // The rest of the file was applied.
                let url = env.origin.url("/data/1");
                ensure!(status_with(&p, &url, KEY).await? == 407);
                ensure!(status_with(&p, &url, KEY2).await? == 200);
                p.finish().await
            },
        ),
        scenario("reload/tls-certificate", |_env| async move {
            let cfg = Config::new();
            let p = Proxy::start_text(&cfg.dir, &cfg.render()).await?;
            let tls = p.tls.context("tls")?;
            // A session open before the reload, and a handshake the client
            // trusts (the original certificate).
            let mut before = Client::tls(tls, true).await?;

            // New certificate: the harness client only trusts the old one.
            let names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
            let ck = rcgen::generate_simple_self_signed(names)?;
            std::fs::write(cfg.dir.join("cert.pem"), ck.cert.pem())?;
            std::fs::write(cfg.dir.join("key.pem"), ck.signing_key.serialize_pem())?;
            let (code, out, err) = reload_cmd(&cfg.dir)?;
            ensure!(code == 0, "{out} {err}");
            ensure!(
                Client::tls(tls, true).await.is_err(),
                "new connections still get the old certificate"
            );
            // The established session is unaffected.
            before
                .roundtrip(b"garbage\r\n\r\n", false)
                .await?
                .expect(400)?;

            // A key that does not match the certificate is refused: the
            // new certificate stays.
            std::fs::write(
                cfg.dir.join("key.pem"),
                crate::harness::certs::localhost().key_pem.as_bytes(),
            )?;
            let (code, _, err) = reload_cmd(&cfg.dir)?;
            ensure!(
                code != 0 && err.contains("running config kept"),
                "{code} {err}"
            );
            ensure!(Client::tls(tls, true).await.is_err());
            drop(before);
            p.finish().await
        }),
        scenario("reload/control-socket-disabled", |_env| async move {
            let cfg = Config::new().set("server", "control_socket", "\"\"");
            let p = Proxy::start_text(&cfg.dir, &cfg.render()).await?;
            let (code, _, err) = reload_cmd(&cfg.dir)?;
            ensure!(code == 1 && err.contains("SIGHUP"), "{code} {err}");
            p.finish().await
        }),
        scenario("reload/control-socket-lifecycle", |_env| async move {
            let cfg = Config::new();
            let sock = cfg.dir.join("ctl.sock");
            // A stale socket file (crash) is replaced at start.
            std::fs::write(&sock, b"")?;
            let p = Proxy::start_text(&cfg.dir, &cfg.render()).await?;
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&sock)?.permissions().mode() & 0o777;
            ensure!(mode == 0o600, "control socket mode {mode:o}");
            ensure!(reload_cmd(&cfg.dir)?.0 == 0);
            // No config file: a clear error, not a hang.
            let (code, _, err) = reload_cmd(Path::new("/nonexistent"))?;
            ensure!(code != 0 && !err.is_empty());
            // SIGTERM removes it (checked before `finish` deletes the directory).
            p.signal(libc::SIGTERM);
            for _ in 0..100 {
                if !sock.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ensure!(!sock.exists(), "socket left behind after SIGTERM");
            Ok(())
        }),
    ]
}
