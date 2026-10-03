//! HTTP/2 specifics: multiplexing, ALPN fallback, stream resets, auth and
//! errors as stream statuses.

use std::time::Duration;

use anyhow::ensure;

use crate::clients::http1::{default_auth, get, Client};
use crate::clients::{basic, h2 as h2c};
use crate::harness::{payload, Config, Proxy};
use crate::runner::{scenario, Scenario};

fn auth() -> Vec<(&'static str, String)> {
    vec![("proxy-authorization", default_auth())]
}

pub fn scenarios() -> Vec<Scenario> {
    vec![
        scenario("h2/multiplexed-mixed-sizes", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let send = h2c::connect(p.tls()).await?;
            let mut tasks = tokio::task::JoinSet::new();
            for i in 0..60usize {
                let mut send = send.clone();
                let n = [10, 9000, 70_000, 300_000, 1 << 20][i % 5] + i;
                let f = ["cl", "chunked", "eof"][i % 3];
                let url = env.origin.url(&format!("/data/{n}?framing={f}"));
                tasks.spawn(async move {
                    let a = auth();
                    let h: Vec<(&str, &str)> = a.iter().map(|(k, v)| (*k, v.as_str())).collect();
                    let r = h2c::request(&mut send, "GET", &url, &h, None).await?;
                    ensure!(r.status == 200, "{url}: {}", r.status);
                    payload::check(n as u64, n, &r.body)
                });
            }
            while let Some(r) = tasks.join_next().await {
                r??;
            }
            drop(send);
            p.finish().await
        })
        .quick(),
        scenario(
            "h2/alpn-http2-disabled-falls-back-to-http11",
            |env| async move {
                let p = Proxy::start(Config::new().set("tls", "http2", "false")).await?;
                let t = crate::clients::tls_connect(
                    p.tls(),
                    crate::harness::certs::client(&[b"h2", b"http/1.1"]),
                )
                .await?;
                ensure!(
                    t.get_ref().1.alpn_protocol() != Some(b"h2"),
                    "h2 negotiated with http2 = false"
                );
                let mut c = Client::over(Box::new(t));
                c.roundtrip(&get(&env.origin.url("/data/100")), false)
                    .await?
                    .expect(200)?;
                drop(c);
                p.finish().await
            },
        ),
        scenario("h2/auth-required-and-wrong-key", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut s = h2c::connect(p.tls()).await?;
            let url = env.origin.url("/data/10");
            let r = h2c::request(&mut s, "GET", &url, &[], None).await?;
            ensure!(r.status == 407, "no credentials: {}", r.status);
            ensure!(
                r.headers.contains_key("proxy-authenticate"),
                "407 without Proxy-Authenticate"
            );
            let bad = basic("u", "wrong");
            let r =
                h2c::request(&mut s, "GET", &url, &[("proxy-authorization", &bad)], None).await?;
            ensure!(r.status == 407, "wrong key: {}", r.status);
            let (st, _) = h2c::connect_tunnel(&mut s, &env.targets.echo.to_string(), None).await?;
            ensure!(st == 407, "CONNECT without credentials: {st}");
            // The connection survives refused streams.
            let a = default_auth();
            let r = h2c::request(&mut s, "GET", &url, &[("proxy-authorization", &a)], None).await?;
            ensure!(r.status == 200, "after refusals: {}", r.status);
            drop(s);
            p.finish().await
        })
        .quick(),
        scenario("h2/head-204-304", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut s = h2c::connect(p.tls()).await?;
            let a = default_auth();
            let h = [("proxy-authorization", a.as_str())];
            let r = h2c::request(&mut s, "HEAD", &env.origin.url("/data/5000"), &h, None).await?;
            ensure!(
                r.status == 200 && r.body.is_empty(),
                "HEAD {} {}",
                r.status,
                r.body.len()
            );
            for code in [204u16, 304] {
                let r = h2c::request(
                    &mut s,
                    "GET",
                    &env.origin.url(&format!("/status/{code}")),
                    &h,
                    None,
                )
                .await?;
                ensure!(
                    r.status == code && r.body.is_empty(),
                    "{code}: {}",
                    r.status
                );
            }
            let r = h2c::request(&mut s, "GET", &env.origin.url("/1xx"), &h, None).await?;
            ensure!(r.status == 200 && r.body == b"ok", "1xx: {}", r.status);
            drop(s);
            p.finish().await
        }),
        scenario("h2/upstream-errors", |env| async move {
            let p = Proxy::start(Config::new().set("timeouts", "idle", "\"1s\"")).await?;
            let mut s = h2c::connect(p.tls()).await?;
            let a = default_auth();
            let h = [("proxy-authorization", a.as_str())];
            let r = h2c::request(&mut s, "GET", "http://127.0.0.1:1/", &h, None).await?;
            ensure!(r.status == 502, "refused: {}", r.status);
            let r = h2c::request(&mut s, "GET", &env.origin.url("/hang"), &h, None).await?;
            ensure!(r.status == 504, "silent origin: {}", r.status);
            let r = h2c::request(&mut s, "GET", &env.origin.url("/reset/1000000"), &h, None).await;
            ensure!(
                r.is_err() || r.as_ref().is_ok_and(|r| r.body.len() < 1_000_000),
                "truncated body delivered as complete"
            );
            let r = h2c::request(&mut s, "GET", &env.origin.url("/data/10"), &h, None).await?;
            ensure!(r.status == 200, "after errors: {}", r.status);
            drop(s);
            p.finish().await
        }),
        scenario("h2/stream-reset-mid-download", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut send = h2c::connect(p.tls()).await?;
            let a = default_auth();
            for _ in 0..5 {
                let req = http::Request::builder()
                    .method("GET")
                    .uri(env.origin.url("/data/30000000"))
                    .header("proxy-authorization", &a)
                    .body(())?;
                let mut s = send.clone().ready().await?;
                let (resp, _) = s.send_request(req, true)?;
                let resp = resp.await?;
                let mut body = resp.into_body();
                let first = body.data().await;
                ensure!(first.is_some(), "no data");
                drop(body); // RST_STREAM(CANCEL)
            }
            let r = h2c::request(
                &mut send,
                "GET",
                &env.origin.url("/data/1000"),
                &[("proxy-authorization", &a)],
                None,
            )
            .await?;
            ensure!(r.status == 200, "after resets: {}", r.status);
            payload::check(1000, 1000, &r.body)?;
            drop(send);
            p.finish().await
        }),
        scenario("h2/tunnel-and-requests-interleaved", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut send = h2c::connect(p.tls()).await?;
            let a = default_auth();
            let (st, t) =
                h2c::connect_tunnel(&mut send, &env.targets.echo.to_string(), Some(&a)).await?;
            ensure!(st == 200);
            let tunnel = tokio::spawn(crate::clients::echo_roundtrip(t.unwrap(), 77, 5 << 20));
            for i in 0..20 {
                let n = 1000 * (i + 1);
                let r = h2c::request(
                    &mut send,
                    "GET",
                    &env.origin.url(&format!("/data/{n}")),
                    &[("proxy-authorization", &a)],
                    None,
                )
                .await?;
                ensure!(r.status == 200);
                payload::check(n as u64, n, &r.body)?;
            }
            tokio::time::timeout(Duration::from_secs(30), tunnel).await???;
            drop(send);
            p.finish().await
        }),
    ]
}
