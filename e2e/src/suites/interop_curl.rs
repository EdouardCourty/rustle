//! Interoperability with the system's curl: every proxy mode a real
//! client uses (HTTP proxy, CONNECT tunnel, HTTPS proxy, HTTP/2 to the
//! proxy, SOCKS5 with remote DNS), bodies checked byte for byte.

use anyhow::ensure;

use crate::clients::curl::{self, CurlResult};
use crate::harness::proxy::KEY;
use crate::harness::{payload, Config, Proxy};
use crate::runner::{scenario, skip, Scenario};

fn ok(r: &CurlResult, status: u16) -> anyhow::Result<()> {
    ensure!(
        r.exit == 0 && r.http_code == status,
        "curl exit {} http {} stderr {}",
        r.exit,
        r.http_code,
        r.stderr
    );
    Ok(())
}

fn need_curl() -> anyhow::Result<&'static curl::Curl> {
    curl::detect().ok_or_else(|| skip("curl not installed"))
}

pub fn scenarios() -> Vec<Scenario> {
    let mut v = Vec::new();
    for n in [0usize, 100_000, 5 << 20] {
        let size = payload::label(n);
        v.push(scenario(
            format!("curl/http-proxy-get/{size}"),
            move |env| async move {
                need_curl()?;
                let p = Proxy::start(Config::new()).await?;
                let proxy = format!("http://u:{KEY}@{}", p.addr);
                let r = curl::run(
                    &p.dir,
                    &[
                        "-x",
                        &proxy,
                        &env.origin.url(&format!("/data/{n}?framing=chunked")),
                    ],
                )
                .await?;
                ok(&r, 200)?;
                payload::check(n as u64, n, &r.body)?;
                p.finish().await
            },
        ));
        v.push(scenario(
            format!("curl/connect-tunnel-get/{size}"),
            move |env| async move {
                need_curl()?;
                let p = Proxy::start(Config::new()).await?;
                let proxy = format!("http://u:{KEY}@{}", p.addr);
                let r = curl::run(
                    &p.dir,
                    &["-p", "-x", &proxy, &env.origin.url(&format!("/data/{n}"))],
                )
                .await?;
                ok(&r, 200)?;
                payload::check(n as u64, n, &r.body)?;
                p.finish().await
            },
        ));
        v.push(scenario(
            format!("curl/https-proxy/{size}"),
            move |env| async move {
                need_curl()?;
                let p = Proxy::start(Config::new()).await?;
                let proxy = format!("https://u:{KEY}@localhost:{}", p.tls().port());
                let ca = p.dir.join("cert.pem");
                let r = curl::run(
                    &p.dir,
                    &[
                        "--proxy-cacert",
                        ca.to_str().unwrap(),
                        "-x",
                        &proxy,
                        &env.origin.url(&format!("/data/{n}")),
                    ],
                )
                .await?;
                ok(&r, 200)?;
                payload::check(n as u64, n, &r.body)?;
                p.finish().await
            },
        ));
        v.push(scenario(
            format!("curl/https-proxy-http2/{size}"),
            move |env| async move {
                if !need_curl()?.proxy_http2 {
                    return Err(skip("curl without --proxy-http2 (needs curl >= 8.1)"));
                }
                let p = Proxy::start(Config::new()).await?;
                let proxy = format!("https://u:{KEY}@localhost:{}", p.tls().port());
                let ca = p.dir.join("cert.pem");
                let r = curl::run(
                    &p.dir,
                    &[
                        "--proxy-http2",
                        "--proxy-cacert",
                        ca.to_str().unwrap(),
                        "-x",
                        &proxy,
                        &env.origin.url(&format!("/data/{n}")),
                    ],
                )
                .await?;
                ok(&r, 200)?;
                payload::check(n as u64, n, &r.body)?;
                p.finish().await
            },
        ));
        v.push(scenario(
            format!("curl/socks5h/{size}"),
            move |env| async move {
                need_curl()?;
                let p = Proxy::start(Config::new()).await?;
                let proxy = format!("socks5h://u:{KEY}@{}", p.socks());
                let url = format!("http://localhost:{}/data/{n}", env.origin.v4.port());
                let r = curl::run(&p.dir, &["-x", &proxy, &url]).await?;
                ok(&r, 200)?;
                payload::check(n as u64, n, &r.body)?;
                p.finish().await
            },
        ));
    }
    v.extend([
        scenario("curl/upload-post-and-chunked", |env| async move {
            need_curl()?;
            let p = Proxy::start(Config::new()).await?;
            let body = payload::make(9, 2 << 20);
            let file = p.dir.join("upload.bin");
            std::fs::write(&file, &body)?;
            let proxy = format!("http://u:{KEY}@{}", p.addr);
            let data = format!("@{}", file.display());
            for extra in [None, Some("Transfer-Encoding: chunked")] {
                let mut args = vec!["-x", &proxy, "--data-binary", &data];
                if let Some(h) = extra {
                    args.extend(["-H", h]);
                }
                let url = env.origin.url("/echo");
                args.push(&url);
                let r = curl::run(&p.dir, &args).await?;
                ok(&r, 200)?;
                let (_, got) = super::matrix::split_echo(&r.body)?;
                payload::check(9, 2 << 20, got)?;
            }
            p.finish().await
        })
        .quick(),
        scenario("curl/https-origin-through-tunnel", |env| async move {
            need_curl()?;
            // HTTPS to an origin through CONNECT: TLS between curl and the
            // origin (certificate verified), the proxy only relays.
            let p = Proxy::start(Config::new()).await?;
            let proxy = format!("http://u:{KEY}@{}", p.addr);
            let ca = p.dir.join("cert.pem");
            for n in [0usize, 70_000, 3 << 20] {
                let url = format!("https://localhost:{}/data/{n}", env.targets.https.port());
                let r = curl::run(
                    &p.dir,
                    &["--cacert", ca.to_str().unwrap(), "-x", &proxy, &url],
                )
                .await?;
                ok(&r, 200)?;
                payload::check(n as u64, n, &r.body)?;
            }
            p.finish().await
        }),
        scenario("curl/auth-required-407", |env| async move {
            need_curl()?;
            let p = Proxy::start(Config::new()).await?;
            let r = curl::run(
                &p.dir,
                &[
                    "-x",
                    &format!("http://{}", p.addr),
                    &env.origin.url("/data/1"),
                ],
            )
            .await?;
            ensure!(r.http_code == 407, "http {}", r.http_code);
            let r = curl::run(
                &p.dir,
                &[
                    "-x",
                    &format!("http://u:wrong@{}", p.addr),
                    &env.origin.url("/data/1"),
                ],
            )
            .await?;
            ensure!(r.http_code == 407, "http {}", r.http_code);
            p.finish().await
        }),
    ]);
    v
}
