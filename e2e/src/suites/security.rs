//! Access control: API keys on every transport, bans, client allowlist,
//! destination policy (private ranges, deny CIDRs, port allowlist).

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::ensure;
use tokio::net::TcpSocket;

use crate::clients::http1::{default_auth, request, Client};
use crate::clients::socks5::{self, Outcome, Target};
use crate::clients::{basic, h2 as h2c};
use crate::harness::proxy::{KEY, KEY2};
use crate::harness::{Config, Proxy};
use crate::runner::{scenario, Scenario};

/// Result of one access attempt, transport-independent.
#[derive(Debug, PartialEq)]
enum Access {
    Ok,
    Status(u16),
    Socks(Outcome),
    Closed,
}

/// GET (or CONNECT for SOCKS5) of `target` over transport `t`.
async fn attempt(
    p: &Proxy,
    t: &str,
    auth: Option<&str>,
    target: SocketAddr,
    by_name: bool,
) -> anyhow::Result<Access> {
    let host = if by_name {
        format!("localhost:{}", target.port())
    } else {
        target.to_string()
    };
    let url = format!("http://{host}/data/10");
    let hdr = auth.map(|k| basic("u", k));
    match t {
        "h1" | "tls" => {
            let c = if t == "h1" {
                Client::plain(p.addr).await
            } else {
                Client::tls(p.tls(), false).await
            };
            let Ok(mut c) = c else {
                return Ok(Access::Closed);
            };
            if c.send(&request("GET", &url, hdr.as_deref(), "", b""))
                .await
                .is_err()
            {
                return Ok(Access::Closed);
            }
            Ok(match c.response(false).await {
                Ok(r) if r.status == 200 => Access::Ok,
                Ok(r) => Access::Status(r.status),
                Err(_) => Access::Closed,
            })
        }
        "h2" => {
            let Ok(mut s) = h2c::connect(p.tls()).await else {
                return Ok(Access::Closed);
            };
            let h: Vec<(&str, &str)> = hdr
                .iter()
                .map(|v| ("proxy-authorization", v.as_str()))
                .collect();
            Ok(match h2c::request(&mut s, "GET", &url, &h, None).await {
                Ok(r) if r.status == 200 => Access::Ok,
                Ok(r) => Access::Status(r.status),
                Err(_) => Access::Closed,
            })
        }
        _ => {
            let tg = if by_name {
                Target::Name("localhost".into(), target.port())
            } else {
                Target::Ip(target)
            };
            let Ok((_s, o)) = socks5::connect(p.socks(), auth.map(|k| ("u", k)), &tg).await else {
                return Ok(Access::Closed);
            };
            Ok(match o {
                Outcome::Rep(0) => Access::Ok,
                Outcome::Closed => Access::Closed,
                o => Access::Socks(o),
            })
        }
    }
}

const ALL: [&str; 4] = ["h1", "tls", "h2", "socks5"];

/// Expected refusal per transport: HTTP status / SOCKS outcome.
fn refusal(t: &str, http: u16, socks: Outcome) -> Access {
    if t == "socks5" {
        Access::Socks(socks)
    } else {
        Access::Status(http)
    }
}

pub fn scenarios() -> Vec<Scenario> {
    let mut v = Vec::new();
    for t in ALL {
        v.push(
            scenario(
                format!("security/auth/missing-wrong-valid/{t}"),
                move |env| async move {
                    let p = Proxy::start(Config::new()).await?;
                    let o = env.origin.v4;
                    let missing = if t == "socks5" {
                        Access::Socks(Outcome::NoMethod)
                    } else {
                        Access::Status(407)
                    };
                    ensure!(
                        attempt(&p, t, None, o, false).await? == missing,
                        "no credentials"
                    );
                    let wrong = if t == "socks5" {
                        Access::Socks(Outcome::AuthFailed)
                    } else {
                        Access::Status(407)
                    };
                    ensure!(
                        attempt(&p, t, Some("nope"), o, false).await? == wrong,
                        "wrong key"
                    );
                    ensure!(
                        attempt(&p, t, Some(KEY), o, false).await? == Access::Ok,
                        "first key"
                    );
                    ensure!(
                        attempt(&p, t, Some(KEY2), o, false).await? == Access::Ok,
                        "second key"
                    );
                    p.finish().await
                },
            )
            .quick(),
        );
        v.push(scenario(
            format!("security/policy/block-private/{t}"),
            move |env| async move {
                let p = Proxy::start(Config::new().set("filter", "block_private", "true")).await?;
                let forbidden = refusal(t, 403, Outcome::Rep(2));
                for target in [
                    "127.0.0.1:80",
                    "10.1.2.3:80",
                    "192.168.1.1:80",
                    "169.254.169.254:80",
                    "100.64.0.1:80",
                    "[::1]:80",
                    "[fd00::1]:80",
                    "[fe80::1]:80",
                    "0.0.0.0:80",
                ] {
                    let a: SocketAddr = target.parse()?;
                    let got = attempt(&p, t, Some(KEY), a, false).await?;
                    ensure!(got == forbidden, "{target}: {got:?}");
                }
                // A public name resolving to a private address (rebinding).
                let got = attempt(&p, t, Some(KEY), env.origin.v4, true).await?;
                ensure!(got == forbidden, "localhost by name: {got:?}");
                p.finish().await
            },
        ));
        v.push(scenario(
            format!("security/policy/deny-cidr/{t}"),
            move |env| async move {
                let p =
                    Proxy::start(Config::new().set("filter", "deny", "[\"127.0.0.0/8\"]")).await?;
                let got = attempt(&p, t, Some(KEY), env.origin.v4, false).await?;
                ensure!(got == refusal(t, 403, Outcome::Rep(2)), "{got:?}");
                p.finish().await
            },
        ));
        v.push(scenario(
            format!("security/policy/allowed-ports/{t}"),
            move |env| async move {
                let cfg = Config::new().set(
                    "filter",
                    "allowed_ports",
                    &format!("[{}]", env.origin2.v4.port()),
                );
                let p = Proxy::start(cfg).await?;
                let got = attempt(&p, t, Some(KEY), env.origin.v4, false).await?;
                ensure!(
                    got == refusal(t, 403, Outcome::Rep(2)),
                    "port not allowed: {got:?}"
                );
                ensure!(
                    attempt(&p, t, Some(KEY), env.origin2.v4, false).await? == Access::Ok,
                    "allowed port"
                );
                p.finish().await
            },
        ));
        v.push(scenario(
            format!("security/allow-clients/outsider-closed/{t}"),
            move |env| async move {
                let p =
                    Proxy::start(Config::new().set("auth", "allow_clients", "[\"10.0.0.0/8\"]"))
                        .await?;
                let got = attempt(&p, t, Some(KEY), env.origin.v4, false).await?;
                ensure!(got == Access::Closed, "outsider got {got:?}");
                p.finish().await
            },
        ));
    }
    v.extend([
        scenario(
            "security/allow-clients/by-source-address",
            |env| async move {
                let p =
                    Proxy::start(Config::new().set("auth", "allow_clients", "[\"127.0.0.2/32\"]"))
                        .await?;
                // From 127.0.0.1: closed. From 127.0.0.2: served.
                let mut c = Client::plain(p.addr).await?;
                let _ = c
                    .send(&crate::clients::http1::get(&env.origin.url("/data/1")))
                    .await;
                ensure!(c.closed(Duration::from_secs(3)).await, "127.0.0.1 served");
                let sock = TcpSocket::new_v4()?;
                if sock.bind("127.0.0.2:0".parse()?).is_err() {
                    return Err(crate::runner::skip("cannot bind 127.0.0.2"));
                }
                let s = sock.connect(p.addr).await?;
                let mut c = Client::over(Box::new(s));
                c.roundtrip(
                    &crate::clients::http1::get(&env.origin.url("/data/1")),
                    false,
                )
                .await?
                .expect(200)?;
                drop(c);
                p.finish().await
            },
        ),
        scenario(
            "security/ban/after-failures-then-expires",
            |env| async move {
                let cfg = Config::new()
                    .set("auth", "max_failures", "3")
                    .set("auth", "failure_window", "\"1m\"")
                    .set("auth", "ban_duration", "\"2s\"");
                let p = Proxy::start(cfg).await?;
                let o = env.origin.v4;
                // Missing credentials never count.
                for _ in 0..6 {
                    ensure!(attempt(&p, "h1", None, o, false).await? == Access::Status(407));
                }
                ensure!(
                    attempt(&p, "h1", Some(KEY), o, false).await? == Access::Ok,
                    "banned by 407s without credentials"
                );
                for _ in 0..3 {
                    ensure!(attempt(&p, "h1", Some("bad"), o, false).await? == Access::Status(407));
                }
                // Banned on every listener, even with a valid key.
                for t in ALL {
                    let got = attempt(&p, t, Some(KEY), o, false).await?;
                    ensure!(got == Access::Closed, "{t} while banned: {got:?}");
                }
                tokio::time::sleep(Duration::from_millis(2500)).await;
                ensure!(
                    attempt(&p, "h1", Some(KEY), o, false).await? == Access::Ok,
                    "ban did not expire"
                );
                p.finish().await
            },
        )
        .quick(),
        scenario(
            "security/ban/socks5-wrong-passwords-count",
            |env| async move {
                let cfg = Config::new().set("auth", "max_failures", "2").set(
                    "auth",
                    "ban_duration",
                    "\"1m\"",
                );
                let p = Proxy::start(cfg).await?;
                for _ in 0..2 {
                    attempt(&p, "socks5", Some("bad"), env.targets.echo, false).await?;
                }
                let got = attempt(&p, "h1", Some(KEY), env.origin.v4, false).await?;
                ensure!(
                    got == Access::Closed,
                    "not banned after SOCKS5 failures: {got:?}"
                );
                p.finish().await
            },
        ),
        scenario("security/auth/malformed-credentials", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let url = env.origin.url("/data/1");
            for hdr in [
                "Basic !!!notbase64",
                "Bearer secret1",
                "Basic",
                "basic dTpzZWNyZXQx",
                "Basic dTpzZWNyZXQx extra",
            ] {
                let mut c = Client::plain(p.addr).await?;
                let r = c
                    .roundtrip(&request("GET", &url, Some(hdr), "", b""), false)
                    .await?;
                // "basic" lowercase scheme is legal (RFC 7235: case-insensitive).
                if hdr.starts_with("basic ") {
                    ensure!(r.status == 200 || r.status == 407, "{hdr}: {}", r.status);
                } else {
                    ensure!(r.status == 407, "{hdr}: {}", r.status);
                }
            }
            p.finish().await
        }),
        scenario(
            "security/auth/key-without-user-and-colon-in-key",
            |env| async move {
                let p = Proxy::start(Config::new().set("auth", "api_keys", "[\"k:with:colons\"]"))
                    .await?;
                let mut c = Client::plain(p.addr).await?;
                let r = c
                    .roundtrip(
                        &request(
                            "GET",
                            &env.origin.url("/data/1"),
                            Some(&basic("", "k:with:colons")),
                            "",
                            b"",
                        ),
                        false,
                    )
                    .await?;
                r.expect(200)?;
                let r = c
                    .roundtrip(
                        &request(
                            "GET",
                            &env.origin.url("/data/1"),
                            Some(&default_auth()),
                            "",
                            b"",
                        ),
                        false,
                    )
                    .await?;
                r.expect(407)?;
                p.finish().await
            },
        ),
        scenario("security/open-relay-when-no-keys", |env| async move {
            let p = Proxy::start(Config::new().set("auth", "api_keys", "[]")).await?;
            for t in ALL {
                let got = attempt(&p, t, None, env.origin.v4, false).await?;
                let got = if t == "socks5" {
                    attempt(&p, t, None, env.targets.echo, false).await?
                } else {
                    got
                };
                ensure!(got == Access::Ok, "{t}: {got:?}");
            }
            p.finish().await
        }),
        scenario(
            "security/policy/checked-before-connecting",
            |_env| async move {
                // A forbidden port is refused without any connection attempt:
                // fast even towards an unroutable address.
                let p = Proxy::start(Config::new().set("filter", "allowed_ports", "[443]")).await?;
                let mut c = Client::plain(p.addr).await?;
                let t0 = std::time::Instant::now();
                c.roundtrip(
                    &request(
                        "GET",
                        "http://192.0.2.1:80/",
                        Some(&default_auth()),
                        "",
                        b"",
                    ),
                    false,
                )
                .await?
                .expect(403)?;
                ensure!(
                    t0.elapsed() < Duration::from_millis(500),
                    "took {:?}",
                    t0.elapsed()
                );
                drop(c);
                p.finish().await
            },
        ),
    ]);
    // Quick subset: every policy on the plain HTTP and SOCKS5 paths (the
    // TLS and h2 variants share their checks).
    for s in &mut v {
        if s.name.ends_with("/h1") || s.name.ends_with("/socks5") {
            s.quick = true;
        }
    }
    v
}
