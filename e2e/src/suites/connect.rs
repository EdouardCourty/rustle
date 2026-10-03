//! CONNECT tunnels (HTTP/1.1 plain and TLS, h2, SOCKS5): server-first
//! protocols, early bytes, half-close in both directions, many concurrent
//! tunnels, idle timeout, failures.

use std::time::{Duration, Instant};

use anyhow::ensure;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::matrix::open_tunnel;
use crate::clients::http1::{connect_req, default_auth, Client};
use crate::clients::socks5::{self, Outcome, Target};
use crate::clients::{closed_within, echo_roundtrip, h2 as h2c};
use crate::harness::proxy::KEY;
use crate::harness::{Config, Proxy};
use crate::runner::{scenario, skip, Scenario};

const TRANSPORTS: &[&str] = &["h1", "tls", "h2", "socks5"];

pub fn scenarios() -> Vec<Scenario> {
    let mut v = Vec::new();
    for &t in TRANSPORTS {
        v.push(scenario(
            format!("connect/server-speaks-first/{t}"),
            move |env| async move {
                let p = Proxy::start(Config::new()).await?;
                let a = env.targets.greeter;
                let auth = default_auth();
                let mut s = open_tunnel(
                    &p,
                    t,
                    &a.to_string(),
                    Target::Ip(a),
                    Some(&auth),
                    Some(("u", KEY)),
                )
                .await?;
                let mut hello = [0u8; 7];
                tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut hello)).await??;
                ensure!(&hello == b"HELLO\r\n", "greeting {hello:?}");
                echo_roundtrip(s, 9, 100_000).await?;
                p.finish().await
            },
        ));
        v.push(scenario(
            format!("connect/target-closes-first/{t}"),
            move |env| async move {
                let p = Proxy::start(Config::new()).await?;
                let a = env.origin.v4;
                let auth = default_auth();
                let mut s = open_tunnel(
                    &p,
                    t,
                    &a.to_string(),
                    Target::Ip(a),
                    Some(&auth),
                    Some(("u", KEY)),
                )
                .await?;
                // The origin answers then closes: EOF must reach the client
                // while its own write side is still open.
                s.write_all(b"GET /close HTTP/1.1\r\nHost: x\r\n\r\n")
                    .await?;
                s.flush().await?;
                let mut out = Vec::new();
                tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await??;
                ensure!(
                    out.ends_with(b"\r\n\r\nok"),
                    "{:?}",
                    String::from_utf8_lossy(&out)
                );
                drop(s);
                p.finish().await
            },
        ));
        v.push(scenario(
            format!("connect/refused/{t}"),
            move |_env| async move {
                let p = Proxy::start(Config::new()).await?;
                refused(
                    &p,
                    t,
                    "127.0.0.1:1",
                    Target::Ip("127.0.0.1:1".parse()?),
                    502,
                    5,
                )
                .await?;
                p.finish().await
            },
        ));
        v.push(scenario(
            format!("connect/dns-failure/{t}"),
            move |_env| async move {
                let p = Proxy::start(Config::new()).await?;
                let name = "does-not-exist.invalid";
                refused(
                    &p,
                    t,
                    &format!("{name}:443"),
                    Target::Name(name.into(), 443),
                    502,
                    4,
                )
                .await?;
                p.finish().await
            },
        ));
        v.push(scenario(
            format!("connect/ipv6-target/{t}"),
            move |env| async move {
                let Some(a) = env.targets.echo6 else {
                    return Err(skip("no IPv6 loopback"));
                };
                let p = Proxy::start(Config::new()).await?;
                let auth = default_auth();
                let s = open_tunnel(
                    &p,
                    t,
                    &format!("[::1]:{}", a.port()),
                    Target::Ip(a),
                    Some(&auth),
                    Some(("u", KEY)),
                )
                .await?;
                echo_roundtrip(s, 4, 300_000).await?;
                p.finish().await
            },
        ));
    }
    v.extend([
        scenario("connect/early-bytes-with-request", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let mut req =
                connect_req(&env.targets.echo.to_string(), Some(&default_auth())).into_bytes();
            req.extend_from_slice(b"early-data");
            c.send(&req).await?;
            let head = c.w.head().await?.ok_or_else(|| anyhow::anyhow!("closed"))?;
            ensure!(
                head.starts_with(b"HTTP/1.1 200"),
                "{}",
                String::from_utf8_lossy(&head)
            );
            let got = c.w.exact(10).await?;
            ensure!(got == b"early-data", "echo {got:?}");
            drop(c);
            p.finish().await
        })
        .quick(),
        scenario("connect/concurrent-1000-tunnels", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut tasks = tokio::task::JoinSet::new();
            for i in 0..1000u64 {
                let (addr, echo) = (p.addr, env.targets.echo);
                tasks.spawn(async move {
                    let mut c = Client::plain(addr).await?;
                    let st = c.connect(&echo.to_string(), Some(&default_auth())).await?;
                    ensure!(st == 200, "CONNECT {st}");
                    echo_roundtrip(c.into_stream()?, i, 4096).await
                });
            }
            let mut n = 0;
            while let Some(r) = tasks.join_next().await {
                r??;
                n += 1;
            }
            ensure!(n == 1000);
            p.finish().await
        })
        .timeout(Duration::from_secs(120)),
        scenario(
            "connect/concurrent-200-h2-streams-one-connection",
            |env| async move {
                let p = Proxy::start(Config::new()).await?;
                let send = h2c::connect(p.tls()).await?;
                let mut tasks = tokio::task::JoinSet::new();
                for i in 0..200u64 {
                    let (mut send, echo) = (send.clone(), env.targets.echo);
                    tasks.spawn(async move {
                        let (st, t) = h2c::connect_tunnel(
                            &mut send,
                            &echo.to_string(),
                            Some(&default_auth()),
                        )
                        .await?;
                        ensure!(st == 200, "CONNECT {st}");
                        echo_roundtrip(t.unwrap(), i, 20_000).await
                    });
                }
                while let Some(r) = tasks.join_next().await {
                    r??;
                }
                drop(send);
                p.finish().await
            },
        ),
        scenario("connect/idle-timeout-closes-tunnel", |env| async move {
            let p = Proxy::start(Config::new().set("timeouts", "idle", "\"1s\"")).await?;
            let mut c = Client::plain(p.addr).await?;
            ensure!(
                c.connect(&env.targets.echo.to_string(), Some(&default_auth()))
                    .await?
                    == 200
            );
            let mut s = c.into_stream()?;
            // Activity keeps it alive past the idle timeout...
            let mut last = Instant::now();
            for _ in 0..4 {
                tokio::time::sleep(Duration::from_millis(400)).await;
                s.write_all(b"x").await?;
                let mut b = [0u8; 1];
                s.read_exact(&mut b).await?;
                last = Instant::now();
            }
            // ...silence closes it, about `idle` after the last byte.
            ensure!(
                closed_within(&mut s, Duration::from_secs(4)).await,
                "idle tunnel kept open"
            );
            let silent = last.elapsed();
            ensure!(
                silent > Duration::from_millis(800),
                "closed after {silent:?} of silence"
            );
            drop(s);
            p.finish().await
        }),
        scenario("connect/tunnel-to-the-proxy-itself", |_env| async move {
            // A tunnel looping back to the proxy works like any target.
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            ensure!(
                c.connect(&p.addr.to_string(), Some(&default_auth()))
                    .await?
                    == 200
            );
            let mut inner = Client::over(c.into_stream()?);
            let r = inner
                .roundtrip(&crate::clients::http1::get("http://127.0.0.1:1/"), false)
                .await?;
            r.expect(502)?;
            drop(inner);
            p.finish().await
        }),
    ]);
    v
}

/// Asserts a refused tunnel: HTTP status `http` or SOCKS5 REP `rep`.
async fn refused(
    p: &Proxy,
    t: &str,
    authority: &str,
    target: Target,
    http: u16,
    rep: u8,
) -> anyhow::Result<()> {
    let auth = default_auth();
    match t {
        "h1" | "tls" => {
            let mut c = if t == "h1" {
                Client::plain(p.addr).await?
            } else {
                Client::tls(p.tls(), false).await?
            };
            let st = c.connect(authority, Some(&auth)).await?;
            ensure!(st == http, "CONNECT status {st}, want {http}");
        }
        "h2" => {
            let mut s = h2c::connect(p.tls()).await?;
            let (st, _) = h2c::connect_tunnel(&mut s, authority, Some(&auth)).await?;
            ensure!(st == http, "h2 CONNECT status {st}, want {http}");
        }
        _ => {
            let (_s, o) = socks5::connect(p.socks(), Some(("u", KEY)), &target).await?;
            ensure!(o == Outcome::Rep(rep), "SOCKS5 {o:?}, want REP {rep}");
        }
    }
    Ok(())
}
