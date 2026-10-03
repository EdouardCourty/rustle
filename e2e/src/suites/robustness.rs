//! Misbehaving clients: aborted transfers, slow readers (backpressure),
//! garbage on every port, connection churn. After each, the proxy must
//! still serve and release every client connection.

use std::time::Duration;

use anyhow::ensure;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::clients::http1::{default_auth, get, request, Client};
use crate::clients::{ends_within, h2 as h2c};
use crate::harness::{payload, Config, Proxy};
use crate::runner::{scenario, Scenario};

/// One good request: the proxy is still healthy.
async fn still_serves(p: &Proxy, url: &str) -> anyhow::Result<()> {
    let mut c = Client::plain(p.addr).await?;
    let r = c.roundtrip(&get(url), false).await?;
    r.expect(200)?;
    Ok(())
}

pub fn scenarios() -> Vec<Scenario> {
    vec![
        scenario("robustness/client-aborts-download", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            for t in ["h1", "tls"] {
                for f in ["cl", "chunked", "eof"] {
                    let mut c = if t == "h1" {
                        Client::plain(p.addr).await?
                    } else {
                        Client::tls(p.tls(), false).await?
                    };
                    c.send(&get(&env
                        .origin
                        .url(&format!("/data/50000000?framing={f}"))))
                        .await?;
                    let mut b = vec![0u8; 1 << 20];
                    let mut got = 0;
                    while got < 2 << 20 {
                        got += c.w.s.read(&mut b).await?;
                    }
                    drop(c);
                }
            }
            still_serves(&p, &env.origin.url("/data/10")).await?;
            p.finish().await
        })
        .quick(),
        scenario("robustness/client-aborts-upload", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let head = format!(
                "POST {} HTTP/1.1\r\nProxy-Authorization: {}\r\nContent-Length: 10000000\r\n\r\n",
                env.origin.url("/echo"),
                default_auth()
            );
            c.send(head.as_bytes()).await?;
            c.send(&payload::make(1, 1 << 20)).await?;
            drop(c);
            still_serves(&p, &env.origin.url("/data/10")).await?;
            p.finish().await
        }),
        scenario("robustness/client-aborts-tunnels", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            for _ in 0..20 {
                let mut c = Client::plain(p.addr).await?;
                ensure!(
                    c.connect(&env.targets.echo.to_string(), Some(&default_auth()))
                        .await?
                        == 200
                );
                let mut s = c.into_stream()?;
                s.write_all(&payload::make(2, 300_000)).await?;
                drop(s); // without reading the echo
            }
            let mut send = h2c::connect(p.tls()).await?;
            for _ in 0..20 {
                let (_, t) = h2c::connect_tunnel(
                    &mut send,
                    &env.targets.echo.to_string(),
                    Some(&default_auth()),
                )
                .await?;
                let mut t = t.unwrap();
                t.write_all(&payload::make(2, 100_000)).await?;
                drop(t);
            }
            drop(send);
            still_serves(&p, &env.origin.url("/data/10")).await?;
            p.finish().await
        }),
        scenario("robustness/slow-reader-backpressure", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let rss0 = p.rss_kb();
            let n = 64 << 20;
            for t in ["h1", "tls"] {
                let mut c = if t == "h1" {
                    Client::plain(p.addr).await?
                } else {
                    Client::tls(p.tls(), false).await?
                };
                c.send(&get(&env.origin.url(&format!("/data/{n}")))).await?;
                // Stall 1.5 s: the origin keeps writing, the proxy must not buffer it.
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let grown = p.rss_kb().saturating_sub(rss0);
                ensure!(
                    grown < 48 * 1024,
                    "{t}: proxy RSS +{grown} KB while the client stalls"
                );
                let r = c.response(false).await?;
                r.expect(200)?;
                payload::check(n as u64, n, &r.body)?;
            }
            p.finish().await
        }),
        scenario("robustness/malformed-requests", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let cases: &[&[u8]] = &[
                b"HELLO\r\n\r\n",
                b"GET\r\n\r\n",
                b"GET http://x/ HTTP/9.9\r\n\r\n",
                b"GET http://x/ HTTP/1.1\r\nNoColonHeader\r\n\r\n",
                b"GET  http://x/  HTTP/1.1\r\n\r\n",
                b"\x00\x01\x02\x03\r\n\r\n",
                b"GET http://x/ HTTP/1.1\r\nHost: a\r\n Folded: line\r\n\r\n",
            ];
            for case in cases {
                let mut c = Client::plain(p.addr).await?;
                c.send(case).await?;
                // No answer (closed) is acceptable too.
                if let Ok(r) = c.response(false).await {
                    ensure!(
                        r.status == 400,
                        "{:?}: status {}",
                        String::from_utf8_lossy(case),
                        r.status
                    );
                }
            }
            still_serves(&p, &env.origin.url("/data/10")).await?;
            p.finish().await
        })
        .quick(),
        scenario("robustness/half-request-then-close", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            for cut in [3usize, 20, 60] {
                let req = get(&env.origin.url("/data/10"));
                let mut s = TcpStream::connect(p.addr).await?;
                s.write_all(&req[..cut.min(req.len())]).await?;
                drop(s);
            }
            still_serves(&p, &env.origin.url("/data/10")).await?;
            p.finish().await
        }),
        scenario(
            "robustness/garbage-on-tls-and-socks-ports",
            |env| async move {
                let p = Proxy::start(Config::new()).await?;
                for addr in [p.tls(), p.socks()] {
                    let mut s = TcpStream::connect(addr).await?;
                    s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await?;
                    // TLS port: an alert, then the close.
                    ensure!(
                        ends_within(&mut s, Duration::from_secs(3)).await,
                        "{addr}: garbage not closed"
                    );
                }
                // h2 negotiated, then garbage instead of the preface.
                let mut t =
                    crate::clients::tls_connect(p.tls(), crate::harness::certs::client(&[b"h2"]))
                        .await?;
                t.write_all(b"this is not an h2 preface at all\r\n\r\n")
                    .await?;
                t.flush().await?;
                // GOAWAY, then the close.
                ensure!(
                    ends_within(&mut t, Duration::from_secs(3)).await,
                    "bad h2 preface: connection kept"
                );
                still_serves(&p, &env.origin.url("/data/10")).await?;
                p.finish().await
            },
        ),
        scenario("robustness/connection-churn-2000", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let fds0 = p.fds();
            let url = env.origin.url("/data/100");
            let mut tasks = tokio::task::JoinSet::new();
            for i in 0..2000usize {
                let (addr, url) = (p.addr, url.clone());
                tasks.spawn(async move {
                    let mut c = Client::plain(addr).await?;
                    let close = if i % 2 == 0 {
                        "Connection: close\r\n"
                    } else {
                        ""
                    };
                    let r = c
                        .roundtrip(
                            &request("GET", &url, Some(&default_auth()), close, b""),
                            false,
                        )
                        .await?;
                    r.expect(200)?;
                    anyhow::Ok(())
                });
                if tasks.len() >= 64 {
                    tasks.join_next().await.unwrap()??;
                }
            }
            while let Some(r) = tasks.join_next().await {
                r??;
            }
            p.wait_for("client connections released", Duration::from_secs(5), |p| {
                p.client_sockets() == 0
            })
            .await?;
            // Pool + pipe cache are bounded: no per-connection fd growth.
            let grown = p.fds().saturating_sub(fds0);
            ensure!(grown < 300, "fds grew by {grown} after 2000 connections");
            p.finish().await
        })
        .timeout(Duration::from_secs(120)),
        scenario("robustness/huge-request-line", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let url = env
                .origin
                .url(&format!("/data/1?pad={}", "p".repeat(70_000)));
            c.send(&get(&url)).await?;
            if let Ok(r) = c.response(false).await {
                ensure!(
                    r.status == 431 || r.status == 414 || r.status == 400,
                    "status {}",
                    r.status
                );
            }
            drop(c);
            still_serves(&p, &env.origin.url("/data/10")).await?;
            p.finish().await
        }),
    ]
}
