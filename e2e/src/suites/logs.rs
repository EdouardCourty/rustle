//! Access log: levels, line content per transport, target modes, stdout
//! output, SIGHUP reopen, aggregated refusals.

use std::path::Path;
use std::time::Duration;

use anyhow::ensure;
use tokio::io::AsyncWriteExt;

use crate::clients::http1::{default_auth, get, request, Client};
use crate::clients::socks5::{self, Outcome, Target};
use crate::clients::{basic, echo_roundtrip, h2 as h2c};
use crate::harness::proxy::KEY;
use crate::harness::{Config, Proxy};
use crate::runner::{scenario, Scenario};

fn log_cfg(level: &str) -> Config {
    let c = Config::new();
    let f = format!("{:?}", c.path("access.log").to_string_lossy());
    c.set("log", "access", &format!("\"{level}\""))
        .set("log", "file", &f)
}

/// Waits until the file holds a line containing every needle.
async fn wait_line(file: &Path, needles: &[&str]) -> anyhow::Result<String> {
    let mut text = String::new();
    for _ in 0..100 {
        text = std::fs::read_to_string(file).unwrap_or_default();
        if text.lines().any(|l| needles.iter().all(|n| l.contains(n))) {
            return Ok(text);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("no line with {needles:?} in:\n{text}")
}

pub fn scenarios() -> Vec<Scenario> {
    vec![
        scenario(
            "logs/all/one-line-per-request-and-tunnel",
            |env| async move {
                let cfg = log_cfg("all");
                let file = cfg.path("access.log");
                let p = Proxy::start(cfg).await?;
                let mut c = Client::plain(p.addr).await?;
                c.roundtrip(&get(&env.origin.url("/data/1234")), false)
                    .await?
                    .expect(200)?;
                wait_line(
                    &file,
                    &[
                        "via=http1",
                        "kind=http",
                        "method=GET",
                        "status=200",
                        "down=1234",
                    ],
                )
                .await?;
                drop(c);

                let mut c = Client::tls(p.tls(), false).await?;
                c.roundtrip(&get(&env.origin.url("/data/10")), false)
                    .await?
                    .expect(200)?;
                wait_line(&file, &["via=tls", "kind=http", "status=200"]).await?;
                drop(c);

                let mut c = Client::plain(p.addr).await?;
                ensure!(
                    c.connect(&env.targets.echo.to_string(), Some(&default_auth()))
                        .await?
                        == 200
                );
                echo_roundtrip(c.into_stream()?, 1, 777).await?;
                wait_line(
                    &file,
                    &[
                        "kind=connect",
                        &format!("target={}", env.targets.echo),
                        "up=777",
                        "down=777",
                    ],
                )
                .await?;

                let mut s = h2c::connect(p.tls()).await?;
                let a = default_auth();
                let r = h2c::request(
                    &mut s,
                    "GET",
                    &env.origin.url("/data/55"),
                    &[("proxy-authorization", &a)],
                    None,
                )
                .await?;
                ensure!(r.status == 200);
                wait_line(&file, &["via=h2", "status=200"]).await?;
                drop(s);

                let (s, o) =
                    socks5::connect(p.socks(), Some(("u", KEY)), &Target::Ip(env.targets.echo))
                        .await?;
                ensure!(o == Outcome::Rep(0));
                echo_roundtrip(s, 2, 99).await?;
                wait_line(&file, &["via=socks5", "kind=connect", "up=99", "down=99"]).await?;

                let (ctl, relay) = socks5::udp_associate(p.socks(), Some(("u", KEY))).await?;
                let u = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
                u.send_to(
                    &socks5::udp_packet(&Target::Ip(env.targets.udp), b"ping"),
                    relay,
                )
                .await?;
                let mut b = [0u8; 64];
                tokio::time::timeout(Duration::from_secs(2), u.recv_from(&mut b)).await??;
                drop(ctl);
                wait_line(&file, &["via=socks5", "kind=udp"]).await?;
                p.finish().await
            },
        )
        .quick(),
        scenario("logs/errors/refusals-only", |env| async move {
            let cfg = log_cfg("errors").set(
                "filter",
                "allowed_ports",
                &format!("[{}]", env.origin.v4.port()),
            );
            let file = cfg.path("access.log");
            let p = Proxy::start(cfg).await?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/data/1")), false)
                .await?
                .expect(200)?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin2.url("/data/1")), false)
                .await?
                .expect(403)?;
            wait_line(&file, &["status=403", "reason=port"]).await?;
            let mut c = Client::plain(p.addr).await?;
            let r = c
                .roundtrip(
                    &request(
                        "GET",
                        &env.origin.url("/"),
                        Some(&basic("u", "bad")),
                        "",
                        b"",
                    ),
                    false,
                )
                .await?;
            r.expect(407)?;
            let text = wait_line(&file, &["event=auth-fail"]).await?;
            ensure!(
                !text.contains("status=200"),
                "200 logged at level errors:\n{text}"
            );
            drop(c);
            p.finish().await
        }),
        scenario("logs/target-url", |env| async move {
            let cfg = log_cfg("all").set("log", "target", "\"url\"");
            let file = cfg.path("access.log");
            let p = Proxy::start(cfg).await?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/data/1?x=y")), false)
                .await?
                .expect(200)?;
            wait_line(
                &file,
                &[&format!("target={}", env.origin.url("/data/1?x=y"))],
            )
            .await?;
            drop(c);
            p.finish().await
        }),
        scenario("logs/stdout-output", |env| async move {
            let p = Proxy::start(Config::new().set("log", "access", "\"all\"").set(
                "log",
                "output",
                "\"stdout\"",
            ))
            .await?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/data/3")), false)
                .await?
                .expect(200)?;
            p.wait_for("access line on stdout", Duration::from_secs(3), |p| {
                p.stdout().contains("kind=http")
            })
            .await?;
            ensure!(!p.stderr().contains("kind=http"), "access lines on stderr");
            drop(c);
            p.finish().await
        }),
        scenario("logs/off-writes-nothing", |env| async move {
            let cfg = log_cfg("off");
            let file = cfg.path("access.log");
            let p = Proxy::start(cfg).await?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/status/500")), false)
                .await?
                .expect(500)?;
            c.roundtrip(&get("http://127.0.0.1:1/"), false)
                .await?
                .expect(502)?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            ensure!(
                std::fs::read_to_string(&file)
                    .unwrap_or_default()
                    .is_empty(),
                "lines written with access = off"
            );
            drop(c);
            p.finish().await
        }),
        scenario("logs/sighup-reopens-after-rename", |env| async move {
            let cfg = log_cfg("all");
            let file = cfg.path("access.log");
            let p = Proxy::start(cfg).await?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/data/11")), false)
                .await?
                .expect(200)?;
            wait_line(&file, &["down=11"]).await?;
            let rotated = file.with_extension("log.1");
            std::fs::rename(&file, &rotated)?;
            p.signal(libc::SIGHUP);
            tokio::time::sleep(Duration::from_millis(300)).await;
            c.roundtrip(&get(&env.origin.url("/data/22")), false)
                .await?
                .expect(200)?;
            wait_line(&file, &["down=22"]).await?;
            ensure!(
                !std::fs::read_to_string(&rotated)?.contains("down=22"),
                "written to the rotated file"
            );
            drop(c);
            p.finish().await
        }),
        scenario("logs/refusals-at-accept-aggregated", |_env| async move {
            let cfg = log_cfg("errors").set("auth", "allow_clients", "[\"10.0.0.0/8\"]");
            let file = cfg.path("access.log");
            let p = Proxy::start(cfg).await?;
            for _ in 0..30 {
                let mut s = tokio::net::TcpStream::connect(p.addr).await?;
                let _ = s.write_all(b"GET / HTTP/1.1\r\n\r\n").await;
            }
            let text = wait_line(&file, &["event=refused", "reason=allowlist"]).await?;
            let lines = text.lines().filter(|l| l.contains("event=refused")).count();
            ensure!(
                lines <= 3,
                "{lines} refusal lines for 30 connections (not aggregated)"
            );
            ensure!(text.contains("last=127.0.0.1"));
            p.finish().await
        }),
    ]
}
