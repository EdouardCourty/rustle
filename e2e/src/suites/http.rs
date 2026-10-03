//! Plain HTTP forwarding semantics (HTTP/1.1 client on the plain port,
//! unless stated): keep-alive and upstream pooling, framing edge cases,
//! retries, upstream failures, header rewriting.

use std::time::{Duration, Instant};

use anyhow::{ensure, Context};

use super::matrix::split_echo;
use crate::clients::http1::{default_auth, get, request, Client};
use crate::harness::{origin, payload, Config, Proxy};
use crate::runner::{scenario, Scenario};

pub fn scenarios() -> Vec<Scenario> {
    vec![
        scenario("http/keepalive/upstream-pool-reused", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut conns = Vec::new();
            // Same client connection, then a second client connection:
            // one upstream connection serves everything (workers = 1).
            for _ in 0..2 {
                let mut c = Client::plain(p.addr).await?;
                for _ in 0..5 {
                    let r = c
                        .roundtrip(&get(&env.origin.url("/data/100")), false)
                        .await?;
                    r.expect(200)?;
                    conns.push(r.header("x-conn").unwrap_or("").to_string());
                }
            }
            conns.dedup();
            ensure!(conns.len() == 1, "upstream connections used: {conns:?}");
            p.finish().await
        })
        .quick(),
        scenario("http/keepalive/upstream-keepalive-off", |env| async move {
            let p = Proxy::start(Config::new().set("http", "upstream_keepalive", "false")).await?;
            let mut c = Client::plain(p.addr).await?;
            let mut conns = Vec::new();
            for _ in 0..4 {
                let r = c
                    .roundtrip(&get(&env.origin.url("/data/100")), false)
                    .await?;
                r.expect(200)?;
                conns.push(r.header("x-conn").unwrap_or("").to_string());
            }
            conns.dedup();
            ensure!(conns.len() == 4, "upstream connection reused: {conns:?}");
            p.finish().await
        }),
        scenario("http/keepalive/client-connection-close", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let req = request(
                "GET",
                &env.origin.url("/data/10"),
                Some(&default_auth()),
                "Connection: close\r\n",
                b"",
            );
            c.roundtrip(&req, false).await?.expect(200)?;
            ensure!(
                c.closed(Duration::from_secs(3)).await,
                "proxy kept the connection open"
            );
            p.finish().await
        })
        .quick(),
        scenario("http/keepalive/http10", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let req = format!(
                "GET {} HTTP/1.0\r\nProxy-Authorization: {}\r\n\r\n",
                env.origin.url("/data/3000"),
                default_auth()
            );
            let r = c.roundtrip(req.as_bytes(), false).await?;
            r.expect(200)?;
            payload::check(3000, 3000, &r.body)?;
            ensure!(
                c.closed(Duration::from_secs(3)).await,
                "HTTP/1.0 connection kept open"
            );
            p.finish().await
        }),
        scenario("http/framing/head", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            for f in ["cl", "chunked"] {
                let url = env.origin.url(&format!("/data/5000?framing={f}"));
                let r = c
                    .roundtrip(&request("HEAD", &url, Some(&default_auth()), "", b""), true)
                    .await?;
                r.expect(200)?;
                // No body may follow: the next response must parse.
                let r = c
                    .roundtrip(&get(&env.origin.url("/data/10")), false)
                    .await?;
                r.expect(200).with_context(|| format!("after HEAD ({f})"))?;
                payload::check(10, 10, &r.body)?;
            }
            p.finish().await
        })
        .quick(),
        scenario("http/framing/204-304-no-body", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            for code in [204u16, 304] {
                let r = c
                    .roundtrip(&get(&env.origin.url(&format!("/status/{code}"))), false)
                    .await?;
                r.expect(code)?;
                let r = c
                    .roundtrip(&get(&env.origin.url("/data/10")), false)
                    .await?;
                r.expect(200).with_context(|| format!("after {code}"))?;
            }
            p.finish().await
        }),
        scenario("http/framing/status-codes", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            for code in [201u16, 301, 302, 400, 401, 403, 404, 418, 500, 502, 503] {
                let r = c
                    .roundtrip(&get(&env.origin.url(&format!("/status/{code}"))), false)
                    .await?;
                r.expect(code)?;
            }
            p.finish().await
        }),
        scenario("http/framing/1xx-interim", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let r = c.roundtrip(&get(&env.origin.url("/1xx")), false).await?;
            r.expect(200)?;
            ensure!(r.interim == [103], "interim responses {:?}", r.interim);
            ensure!(r.body == b"ok");
            c.roundtrip(&get(&env.origin.url("/data/10")), false)
                .await?
                .expect(200)?;
            p.finish().await
        }),
        scenario(
            "http/framing/chunked-trailers-preserved",
            |env| async move {
                let p = Proxy::start(Config::new()).await?;
                let mut c = Client::plain(p.addr).await?;
                let r = c
                    .roundtrip(
                        &get(&env
                            .origin
                            .url("/data/70000?framing=chunked&chunk=7000&trailers=1")),
                        false,
                    )
                    .await?;
                r.expect(200)?;
                payload::check(70000, 70000, &r.body)?;
                ensure!(r.trailers == ["X-Sum: done"], "trailers {:?}", r.trailers);
                c.roundtrip(&get(&env.origin.url("/data/10")), false)
                    .await?
                    .expect(200)?;
                p.finish().await
            },
        ),
        scenario("http/framing/slow-origin", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let r = c
                .roundtrip(&get(&env.origin.url("/slow/200000?ms=30")), false)
                .await?;
            r.expect(200)?;
            payload::check(200000, 200000, &r.body)?;
            p.finish().await
        }),
        scenario("http/retry/stale-pooled-get-retried", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            // The origin closes this pooled connection once idle 100 ms.
            c.roundtrip(&get(&env.origin.url("/close-idle")), false)
                .await?
                .expect(200)?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let r = c
                .roundtrip(&get(&env.origin.url("/data/1000")), false)
                .await?;
            r.expect(200)?;
            payload::check(1000, 1000, &r.body)?;
            p.finish().await
        })
        .quick(),
        scenario("http/retry/get-dropped-retried-once", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/data/1")), false)
                .await?
                .expect(200)?;
            // Origin reads the GET and closes on a reused connection:
            // one retry on a fresh connection, then 502.
            let r = c
                .roundtrip(&get(&env.origin.url("/drop?key=retry-get")), false)
                .await?;
            r.expect(502)?;
            ensure!(
                origin::hits("retry-get") == 2,
                "GET hits {}",
                origin::hits("retry-get")
            );
            p.finish().await
        }),
        scenario("http/retry/post-never-replayed", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/data/1")), false)
                .await?
                .expect(200)?;
            let url = env.origin.url("/drop?key=retry-post");
            let r = c
                .roundtrip(
                    &request("POST", &url, Some(&default_auth()), "", b"payload"),
                    false,
                )
                .await?;
            r.expect(502)?;
            ensure!(
                origin::hits("retry-post") == 1,
                "POST replayed: {} hits",
                origin::hits("retry-post")
            );
            p.finish().await
        })
        .quick(),
        scenario("http/upstream/reset-mid-body", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            c.send(&get(&env.origin.url("/reset/2000000"))).await?;
            let r = c.response(false).await;
            // Content-Length promised, connection cut short: the client
            // must see the truncation (never a complete-looking response).
            ensure!(r.is_err(), "truncated body delivered as complete");
            drop(c);
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/data/10")), false)
                .await?
                .expect(200)?;
            p.finish().await
        }),
        scenario("http/upstream/silent-origin-504", |env| async move {
            let p = Proxy::start(Config::new().set("timeouts", "idle", "\"1s\"")).await?;
            let mut c = Client::plain(p.addr).await?;
            let t0 = Instant::now();
            let r = c.roundtrip(&get(&env.origin.url("/hang")), false).await?;
            r.expect(504)?;
            ensure!(
                t0.elapsed() < Duration::from_secs(4),
                "504 after {:?}",
                t0.elapsed()
            );
            p.finish().await
        }),
        scenario("http/upstream/refused-502", |_env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get("http://127.0.0.1:1/x"), false)
                .await?
                .expect(502)?;
            p.finish().await
        })
        .quick(),
        scenario("http/upstream/dns-failure-502", |_env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get("http://does-not-exist.invalid/x"), false)
                .await?
                .expect(502)?;
            p.finish().await
        }),
        scenario("http/upstream/connect-timeout", |_env| async move {
            let p = Proxy::start(Config::new().set("timeouts", "connect", "\"1s\"")).await?;
            let mut c = Client::plain(p.addr).await?;
            let t0 = Instant::now();
            // TEST-NET-1: unroutable (timeout) or unreachable (error).
            let r = c.roundtrip(&get("http://192.0.2.1:80/x"), false).await?;
            ensure!(matches!(r.status, 502 | 504), "status {}", r.status);
            ensure!(
                t0.elapsed() < Duration::from_secs(4),
                "took {:?}",
                t0.elapsed()
            );
            p.finish().await
        }),
        scenario("http/target/ipv6-literal", |env| async move {
            let Some(v6) = env.origin.v6 else {
                return Err(crate::runner::skip("no IPv6 loopback"));
            };
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let r = c
                .roundtrip(&get(&format!("http://[::1]:{}/data/500", v6.port())), false)
                .await?;
            r.expect(200)?;
            payload::check(500, 500, &r.body)?;
            p.finish().await
        }),
        scenario("http/target/hostname", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let r = c
                .roundtrip(
                    &get(&format!(
                        "http://localhost:{}/data/500",
                        env.origin.v4.port()
                    )),
                    false,
                )
                .await?;
            r.expect(200)?;
            p.finish().await
        }),
        scenario("http/target/origin-form-with-host", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let req = format!(
                "GET /data/700 HTTP/1.1\r\nHost: {}\r\nProxy-Authorization: {}\r\n\r\n",
                env.origin.auth(),
                default_auth()
            );
            let r = c.roundtrip(req.as_bytes(), false).await?;
            r.expect(200)?;
            payload::check(700, 700, &r.body)?;
            p.finish().await
        }),
        scenario("http/rewrite/headers-and-request-line", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let url = env.origin.url("/echo?a=1&b=%20x");
            let extra = "Proxy-Connection: keep-alive\r\nX-Custom: kept\r\nUser-Agent: e2e\r\n";
            let r = c
                .roundtrip(
                    &request("GET", &url, Some(&default_auth()), extra, b""),
                    false,
                )
                .await?;
            r.expect(200)?;
            let (head, _) = split_echo(&r.body)?;
            let lower = head.to_ascii_lowercase();
            ensure!(
                head.starts_with("GET /echo?a=1&b=%20x HTTP/1.1\r\n"),
                "request line: {head}"
            );
            ensure!(
                !lower.contains("proxy-authorization"),
                "Proxy-Authorization forwarded:\n{head}"
            );
            ensure!(
                !lower.contains("proxy-connection"),
                "Proxy-Connection forwarded:\n{head}"
            );
            ensure!(
                lower.contains("x-custom: kept"),
                "custom header lost:\n{head}"
            );
            ensure!(
                lower.contains(&format!("host: {}", env.origin.auth())),
                "Host:\n{head}"
            );
            p.finish().await
        })
        .quick(),
        scenario("http/rewrite/methods", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            for m in ["PUT", "DELETE", "OPTIONS", "PATCH", "PROPFIND"] {
                let body: &[u8] = if m == "PUT" || m == "PATCH" {
                    b"data"
                } else {
                    b""
                };
                let r = c
                    .roundtrip(
                        &request(m, &env.origin.url("/echo"), Some(&default_auth()), "", body),
                        false,
                    )
                    .await?;
                r.expect(200).with_context(|| m.to_string())?;
                let (head, got) = split_echo(&r.body)?;
                ensure!(head.starts_with(&format!("{m} /echo ")), "{m}: {head}");
                ensure!(got == body, "{m}: body {got:?}");
            }
            p.finish().await
        }),
        scenario("http/response/large-head", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let r = c
                .roundtrip(&get(&env.origin.url("/headers/12000")), false)
                .await?;
            r.expect(200)?;
            ensure!(r.header("x-big").map(str::len) == Some(12000));
            p.finish().await
        }),
        scenario("http/request/many-headers", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let extra: String = (0..50).map(|i| format!("X-H{i}: v{i}\r\n")).collect();
            let r = c
                .roundtrip(
                    &request(
                        "GET",
                        &env.origin.url("/echo"),
                        Some(&default_auth()),
                        &extra,
                        b"",
                    ),
                    false,
                )
                .await?;
            r.expect(200)?;
            let (head, _) = split_echo(&r.body)?;
            ensure!(head.contains("X-H49: v49"), "headers lost");
            p.finish().await
        }),
        scenario("http/request/pipelined", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let mut two = get(&env.origin.url("/data/100"));
            two.extend(get(&env.origin.url("/data/200")));
            c.send(&two).await?;
            let r = c.response(false).await?;
            r.expect(200)?;
            payload::check(100, 100, &r.body)?;
            // Pipelining is not supported: either the second response or
            // a clean close, never garbage.
            match c.response(false).await {
                Ok(r2) => {
                    r2.expect(200)?;
                    payload::check(200, 200, &r2.body)?;
                }
                Err(_) => ensure!(c.w.buf.is_empty(), "garbage after the first response"),
            }
            p.finish().await
        }),
        scenario("http/request/expect-100-continue", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let body = payload::make(5, 5000);
            let req = request(
                "POST",
                &env.origin.url("/echo"),
                Some(&default_auth()),
                "Expect: 100-continue\r\n",
                &body,
            );
            let r = c.roundtrip(&req, false).await?;
            r.expect(200)?;
            let (_, got) = split_echo(&r.body)?;
            payload::check(5, 5000, got)?;
            p.finish().await
        }),
        scenario("http/request/absolute-uri-without-path", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let r = c
                .roundtrip(
                    &request(
                        "GET",
                        &format!("http://{}", env.origin.auth()),
                        Some(&default_auth()),
                        "",
                        b"",
                    ),
                    false,
                )
                .await?;
            // `/` on the origin: 404 from the origin itself, not a proxy error.
            r.expect(404)?;
            ensure!(
                r.has_header("x-conn"),
                "answer did not come from the origin"
            );
            p.finish().await
        }),
        scenario(
            "http/transport/tls-client-same-semantics",
            |env| async move {
                let p = Proxy::start(Config::new()).await?;
                let mut c = Client::tls(p.tls(), false).await?;
                for path in [
                    "/1xx",
                    "/status/204",
                    "/data/9000?framing=chunked",
                    "/data/9000",
                ] {
                    let r = c.roundtrip(&get(&env.origin.url(path)), false).await?;
                    ensure!(r.status < 300, "{path}: {}", r.status);
                }
                c.roundtrip(
                    &request(
                        "HEAD",
                        &env.origin.url("/data/5"),
                        Some(&default_auth()),
                        "",
                        b"",
                    ),
                    true,
                )
                .await?
                .expect(200)?;
                c.roundtrip(&get(&env.origin.url("/data/5")), false)
                    .await?
                    .expect(200)?;
                p.finish().await
            },
        ),
        scenario("http/transport/health-endpoint", |_env| async move {
            let p = Proxy::start(Config::new()).await?;
            let admin = p.admin.context("admin listener")?;
            ensure!(crate::harness::proxy::health(admin).await? == 200);
            // The admin port never proxies.
            let mut c = Client::plain(admin).await?;
            c.send(&get("http://127.0.0.1:1/")).await?;
            let r = c.response(false).await?;
            ensure!(r.status == 404, "admin answered {}", r.status);
            p.finish().await
        }),
    ]
}
