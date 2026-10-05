//! WebSocket handshakes on the plain-HTTP path (`Upgrade: websocket`) and
//! the `http.allow_websocket` switch.

use std::time::Duration;

use anyhow::{ensure, Context};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::clients::http1::{default_auth, request, Client};
use crate::clients::BoxIo;
use crate::harness::origin;
use crate::harness::{payload, Config, Proxy};
use crate::runner::{scenario, Scenario};

const HANDSHAKE: &str = "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n";

/// Reads until the end of a response head; returns everything read so far.
async fn read_head(s: &mut BoxIo) -> anyhow::Result<Vec<u8>> {
    let mut got = Vec::new();
    let mut b = [0u8; 4096];
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut b))
            .await
            .context("no response head")??;
        ensure!(
            n > 0,
            "EOF before a response head: {}",
            String::from_utf8_lossy(&got)
        );
        got.extend_from_slice(&b[..n]);
    }
    Ok(got)
}

/// Sends the handshake for `path` over `c`; returns the raw stream and the
/// bytes read up to and including the response head (plus anything after).
async fn handshake(
    mut c: Client,
    origin_url: &str,
    auth: Option<&str>,
) -> anyhow::Result<(BoxIo, Vec<u8>)> {
    c.send(&request("GET", origin_url, auth, HANDSHAKE, b""))
        .await?;
    let mut s = c.into_stream()?;
    let got = read_head(&mut s).await?;
    Ok((s, got))
}

async fn echo_roundtrip(s: &mut BoxIo, n: usize) -> anyhow::Result<()> {
    let data = payload::make(7, n);
    let (mut r, mut w) = tokio::io::split(s);
    let d2 = data.clone();
    let writer = async move {
        w.write_all(&d2).await?;
        w.flush().await?;
        anyhow::Ok(())
    };
    let reader = async {
        let mut got = vec![0u8; n];
        r.read_exact(&mut got).await?;
        anyhow::Ok(got)
    };
    let ((), got) = tokio::try_join!(writer, reader)?;
    ensure!(got == data, "echo mismatch ({n} bytes)");
    Ok(())
}

pub fn scenarios() -> Vec<Scenario> {
    vec![
        scenario("websocket/echo-through-proxy", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let auth = default_auth();
            let c = Client::plain(p.addr).await?;
            let (mut s, head) = handshake(c, &env.origin.url("/ws"), Some(&auth)).await?;
            let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
            ensure!(text.starts_with("http/1.1 101"), "{text}");
            ensure!(
                text.contains("upgrade: websocket") && text.contains("sec-websocket-accept"),
                "{text}"
            );
            echo_roundtrip(&mut s, 1_000).await?;
            echo_roundtrip(&mut s, 400_000).await?;
            drop(s);
            p.finish().await
        })
        .quick(),
        scenario("websocket/over-the-https-proxy", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let auth = default_auth();
            let c = Client::tls(p.tls.context("tls")?, true).await?;
            let (mut s, head) = handshake(c, &env.origin.url("/ws"), Some(&auth)).await?;
            ensure!(
                head.starts_with(b"HTTP/1.1 101"),
                "{}",
                String::from_utf8_lossy(&head)
            );
            echo_roundtrip(&mut s, 200_000).await?;
            drop(s);
            p.finish().await
        }),
        scenario(
            "websocket/handshake-headers-reach-the-origin",
            |env| async move {
                let p = Proxy::start(Config::new()).await?;
                let auth = default_auth();
                let c = Client::plain(p.addr).await?;
                let (mut s, mut got) =
                    handshake(c, &env.origin.url("/ws?mode=head"), Some(&auth)).await?;
                // The origin repeats the request head it received behind its 101.
                let mut b = [0u8; 4096];
                while !String::from_utf8_lossy(&got).contains("Sec-WebSocket-Key") {
                    let n = s.read(&mut b).await?;
                    ensure!(n > 0, "EOF");
                    got.extend_from_slice(&b[..n]);
                }
                let text = String::from_utf8_lossy(&got).into_owned();
                let seen = text
                    .split("\r\n\r\n")
                    .nth(1)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                ensure!(seen.starts_with("get /ws?mode=head http/1.1"), "{seen}");
                ensure!(
                    seen.contains("connection: upgrade"),
                    "Connection not upgraded: {seen}"
                );
                ensure!(seen.contains("upgrade: websocket"), "{seen}");
                ensure!(
                    seen.contains("sec-websocket-key: dghlihnhbxbszsbub25jzq=="),
                    "{seen}"
                );
                ensure!(
                    !seen.contains("proxy-authorization"),
                    "credentials leaked: {seen}"
                );
                drop(s);
                p.finish().await
            },
        ),
        scenario(
            "websocket/early-frame-with-the-101-is-delivered",
            |env| async move {
                let p = Proxy::start(Config::new()).await?;
                let auth = default_auth();
                let c = Client::plain(p.addr).await?;
                let (mut s, mut got) =
                    handshake(c, &env.origin.url("/ws?mode=early"), Some(&auth)).await?;
                let mut b = [0u8; 64];
                while !got.ends_with(b"early-frame") {
                    let n = s.read(&mut b).await?;
                    ensure!(n > 0, "EOF: {}", String::from_utf8_lossy(&got));
                    got.extend_from_slice(&b[..n]);
                }
                drop(s);
                p.finish().await
            },
        ),
        scenario("websocket/origin-refusal-is-relayed", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let auth = default_auth();
            let mut c = Client::plain(p.addr).await?;
            c.send(&request(
                "GET",
                &env.origin.url("/ws?mode=refuse"),
                Some(&auth),
                HANDSHAKE,
                b"",
            ))
            .await?;
            let r = c.response(false).await?;
            r.expect(426)?;
            ensure!(r.body == b"nope", "{:?}", r.body);
            ensure!(
                c.closed(Duration::from_secs(2)).await,
                "connection kept after a refused upgrade"
            );
            drop(c);
            p.finish().await
        }),
        scenario("websocket/disabled-by-allow_websocket", |env| async move {
            let p = Proxy::start(Config::new().set("http", "allow_websocket", "false")).await?;
            let auth = default_auth();
            let key = "wsoff";
            let mut c = Client::plain(p.addr).await?;
            c.send(&request(
                "GET",
                &env.origin.url(&format!("/ws?key={key}")),
                Some(&auth),
                HANDSHAKE,
                b"",
            ))
            .await?;
            c.response(false).await?.expect(403)?;
            ensure!(origin::hits(key) == 0, "the handshake reached the origin");
            drop(c);
            // Plain HTTP keeps working.
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(
                &request("GET", &env.origin.url("/data/10"), Some(&auth), "", b""),
                false,
            )
            .await?
            .expect(200)?;
            drop(c);
            p.finish().await
        })
        .quick(),
        scenario("websocket/needs-credentials", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            c.send(&request(
                "GET",
                &env.origin.url("/ws?key=wsnoauth"),
                None,
                HANDSHAKE,
                b"",
            ))
            .await?;
            c.response(false).await?.expect(407)?;
            ensure!(origin::hits("wsnoauth") == 0);
            drop(c);
            p.finish().await
        }),
        scenario("websocket/destination-policy-applies", |env| async move {
            let p = Proxy::start(Config::new().set("filter", "block_private", "true")).await?;
            let auth = default_auth();
            let mut c = Client::plain(p.addr).await?;
            c.send(&request(
                "GET",
                &env.origin.url("/ws?key=wspriv"),
                Some(&auth),
                HANDSHAKE,
                b"",
            ))
            .await?;
            c.response(false).await?.expect(403)?;
            ensure!(origin::hits("wspriv") == 0);
            drop(c);
            p.finish().await
        }),
    ]
}
