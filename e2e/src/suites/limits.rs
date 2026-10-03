//! Configured limits: header size, body budgets, global connection cap,
//! slowloris and handshake timeouts.

use std::time::{Duration, Instant};

use anyhow::ensure;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::matrix::split_echo;
use crate::clients::http1::{self, default_auth, get, request, Client};
use crate::clients::{closed_within, h2 as h2c};
use crate::harness::{payload, Config, Proxy};
use crate::runner::{scenario, Scenario};

pub fn scenarios() -> Vec<Scenario> {
    vec![
        scenario("limits/max-header-bytes-431", |env| async move {
            let p = Proxy::start(Config::new().set("limits", "max_header_bytes", "4096")).await?;
            let mut c = Client::plain(p.addr).await?;
            let big = format!("X-Big: {}\r\n", "a".repeat(8000));
            c.roundtrip(
                &request(
                    "GET",
                    &env.origin.url("/data/1"),
                    Some(&default_auth()),
                    &big,
                    b"",
                ),
                false,
            )
            .await?
            .expect(431)?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/data/1")), false)
                .await?
                .expect(200)?;
            p.finish().await
        })
        .quick(),
        scenario("limits/request-body-content-length-413", |env| async move {
            let p =
                Proxy::start(Config::new().set("http", "max_request_body_bytes", "1000")).await?;
            let url = env.origin.url("/echo");
            let mut c = Client::plain(p.addr).await?;
            let r = c
                .roundtrip(
                    &request("POST", &url, Some(&default_auth()), "", &[b'x'; 1000]),
                    false,
                )
                .await?;
            r.expect(200)?;
            let r = c
                .roundtrip(
                    &request("POST", &url, Some(&default_auth()), "", &[b'x'; 1001]),
                    false,
                )
                .await?;
            r.expect(413)?;
            p.finish().await
        }),
        scenario("limits/request-body-chunked-cut", |env| async move {
            let p =
                Proxy::start(Config::new().set("http", "max_request_body_bytes", "1000")).await?;
            let mut c = Client::plain(p.addr).await?;
            let mut req = format!(
                "POST {} HTTP/1.1\r\nProxy-Authorization: {}\r\nTransfer-Encoding: chunked\r\n\r\n",
                env.origin.url("/echo"),
                default_auth()
            )
            .into_bytes();
            req.extend(http1::chunked(&[b'y'; 5000], 500, None));
            let _ = c.send(&req).await;
            c.response(false).await?.expect(413)?;
            drop(c);
            p.finish().await
        }),
        scenario("limits/request-body-h2-413", |env| async move {
            let p =
                Proxy::start(Config::new().set("http", "max_request_body_bytes", "1000")).await?;
            let mut s = h2c::connect(p.tls()).await?;
            let a = default_auth();
            let r = h2c::request(
                &mut s,
                "POST",
                &env.origin.url("/echo"),
                &[("proxy-authorization", &a)],
                Some((&[b'x'; 2000], true)),
            )
            .await;
            ensure!(
                r.as_ref().map_or(true, |r| r.status == 413),
                "status {:?}",
                r.map(|r| r.status)
            );
            drop(s);
            p.finish().await
        }),
        scenario("limits/response-body-declared-502", |env| async move {
            let p =
                Proxy::start(Config::new().set("http", "max_response_body_bytes", "1000")).await?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/data/1000")), false)
                .await?
                .expect(200)?;
            c.roundtrip(&get(&env.origin.url("/data/5000")), false)
                .await?
                .expect(502)?;
            p.finish().await
        }),
        scenario(
            "limits/response-body-unknown-length-cut",
            |env| async move {
                let p =
                    Proxy::start(Config::new().set("http", "max_response_body_bytes", "100000"))
                        .await?;
                for f in ["chunked", "eof"] {
                    let mut c = Client::plain(p.addr).await?;
                    c.send(&get(&env.origin.url(&format!("/data/1000000?framing={f}"))))
                        .await?;
                    // Err = cut mid-body: the expected outcome.
                    if let Ok(r) = c.response(false).await {
                        ensure!(
                            r.status != 200 || r.body.len() < 1_000_000,
                            "{f}: over-budget body delivered"
                        );
                    }
                }
                p.finish().await
            },
        ),
        scenario(
            "limits/max-connections-global-across-shards",
            |env| async move {
                let cfg = Config::new()
                    .set("limits", "max_connections", "10")
                    .set("server", "workers", "2");
                let p = Proxy::start(cfg).await?;
                let mut held = Vec::new();
                for _ in 0..10 {
                    // A partial head: accepted (past DEFER_ACCEPT), slot held.
                    let mut s = TcpStream::connect(p.addr).await?;
                    s.write_all(b"GET").await?;
                    held.push(s);
                }
                p.wait_for("10 connections accepted", Duration::from_secs(5), |p| {
                    p.client_sockets() >= 10
                })
                .await?;
                tokio::time::sleep(Duration::from_millis(200)).await;
                let mut c = Client::plain(p.addr).await?;
                let r = c.roundtrip(&get(&env.origin.url("/data/1")), false).await;
                ensure!(
                    r.as_ref().map_or(true, |r| r.status == 503),
                    "11th connection served: {:?}",
                    r.map(|r| r.status)
                );
                drop(held);
                tokio::time::sleep(Duration::from_millis(300)).await;
                let mut c = Client::plain(p.addr).await?;
                c.roundtrip(&get(&env.origin.url("/data/1")), false)
                    .await?
                    .expect(200)?;
                drop(c);
                p.finish().await
            },
        )
        .quick(),
        scenario("limits/slowloris-request-head", |env| async move {
            let p = Proxy::start(Config::new().set("timeouts", "request_head", "\"1s\"")).await?;
            let mut s = TcpStream::connect(p.addr).await?;
            let req = String::from_utf8(get(&env.origin.url("/data/1")))?;
            let t0 = Instant::now();
            let mut closed = false;
            for b in req.bytes() {
                if s.write_all(&[b]).await.is_err() {
                    closed = true;
                    break;
                }
                if closed_within(&mut s, Duration::from_millis(150)).await {
                    closed = true;
                    break;
                }
            }
            ensure!(closed, "trickled head never cut");
            ensure!(
                t0.elapsed() < Duration::from_secs(4),
                "cut after {:?}",
                t0.elapsed()
            );
            drop(s);
            p.finish().await
        }),
        scenario("limits/slowloris-socks5-handshake", |_env| async move {
            let p = Proxy::start(Config::new().set("timeouts", "request_head", "\"1s\"")).await?;
            let mut s = TcpStream::connect(p.socks()).await?;
            s.write_all(&[5]).await?;
            ensure!(
                closed_within(&mut s, Duration::from_secs(4)).await,
                "stalled SOCKS5 handshake kept"
            );
            drop(s);
            p.finish().await
        }),
        scenario("limits/tls-handshake-timeout", |_env| async move {
            let p = Proxy::start(Config::new().set("tls", "handshake_timeout", "\"1s\"")).await?;
            let mut s = TcpStream::connect(p.tls()).await?;
            // First byte of a ClientHello record, then nothing.
            s.write_all(&[0x16]).await?;
            let t0 = Instant::now();
            ensure!(
                closed_within(&mut s, Duration::from_secs(5)).await,
                "stalled TLS handshake kept"
            );
            ensure!(t0.elapsed() < Duration::from_secs(4));
            drop(s);
            p.finish().await
        }),
        scenario("limits/generous-limits-do-not-cut", |env| async move {
            let cfg = Config::new()
                .set("http", "max_request_body_bytes", "3000000")
                .set("http", "max_response_body_bytes", "3000000");
            let p = Proxy::start(cfg).await?;
            let mut c = Client::plain(p.addr).await?;
            for f in ["cl", "chunked", "eof"] {
                let mut c2 = Client::plain(p.addr).await?;
                let r = c2
                    .roundtrip(
                        &get(&env.origin.url(&format!("/data/3000000?framing={f}"))),
                        false,
                    )
                    .await?;
                r.expect(200)?;
                payload::check(3_000_000, 3_000_000, &r.body)?;
            }
            // The echo (head + body) must fit the 3 MB response budget too.
            let body = payload::make(1, 2_990_000);
            let r = c
                .roundtrip(
                    &request(
                        "POST",
                        &env.origin.url("/echo"),
                        Some(&default_auth()),
                        "",
                        &body,
                    ),
                    false,
                )
                .await?;
            r.expect(200)?;
            payload::check(1, 2_990_000, split_echo(&r.body)?.1)?;
            p.finish().await
        }),
    ]
}
