//! Request bodies: chunked uploads (extensions, trailers, big chunks, slow
//! senders), invalid framings, bytes after the final chunk.

use std::time::Duration;

use anyhow::ensure;

use super::matrix::split_echo;
use crate::clients::http1::{self, default_auth, get, Client};
use crate::harness::{payload, Config, Proxy};
use crate::runner::{scenario, Scenario};

fn post_head(url: &str, extra: &str) -> Vec<u8> {
    format!(
        "POST {url} HTTP/1.1\r\nHost: x\r\nProxy-Authorization: {}\r\n{extra}\r\n",
        default_auth()
    )
    .into_bytes()
}

pub fn scenarios() -> Vec<Scenario> {
    vec![
        scenario("uploads/chunked-extensions-and-trailers", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let mut req = post_head(&env.origin.url("/echo"), "Transfer-Encoding: chunked\r\n");
            req.extend_from_slice(b"5;name=value\r\nhello\r\n6;x\r\n world\r\n0\r\nX-Trailer: t\r\nX-Other: o\r\n\r\n");
            let r = c.roundtrip(&req, false).await?;
            r.expect(200)?;
            let (_, body) = split_echo(&r.body)?;
            ensure!(body == b"hello world", "{:?}", String::from_utf8_lossy(body));
            c.roundtrip(&get(&env.origin.url("/data/10")), false).await?.expect(200)?;
            p.finish().await
        })
        .quick(),
        scenario("uploads/chunked-one-huge-chunk", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let body = payload::make(42, 12 << 20);
            let mut req = post_head(&env.origin.url("/echo"), "Transfer-Encoding: chunked\r\n");
            req.extend(http1::chunked(&body, body.len(), None));
            let r = c.roundtrip(&req, false).await?;
            r.expect(200)?;
            payload::check(42, 12 << 20, split_echo(&r.body)?.1)?;
            p.finish().await
        }),
        scenario("uploads/chunked-uppercase-hex-and-tiny-chunks", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let mut req = post_head(&env.origin.url("/echo"), "Transfer-Encoding: chunked\r\n");
            let body = payload::make(3, 300);
            for b in body.chunks(1) {
                req.extend_from_slice(b"1\r\n");
                req.extend_from_slice(b);
                req.extend_from_slice(b"\r\n");
            }
            req.extend_from_slice(b"A\r\n0123456789\r\n0\r\n\r\n");
            let r = c.roundtrip(&req, false).await?;
            r.expect(200)?;
            let (_, got) = split_echo(&r.body)?;
            ensure!(got.len() == 310 && got[..300] == body[..] && &got[300..] == b"0123456789");
            p.finish().await
        }),
        scenario("uploads/slow-sender", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let body = payload::make(8, 100_000);
            c.send(&post_head(&env.origin.url("/echo"), &format!("Content-Length: {}\r\n", body.len()))).await?;
            for piece in body.chunks(10_000) {
                tokio::time::sleep(Duration::from_millis(50)).await;
                c.send(piece).await?;
            }
            let r = c.response(false).await?;
            r.expect(200)?;
            payload::check(8, 100_000, split_echo(&r.body)?.1)?;
            p.finish().await
        }),
        scenario("uploads/invalid/chunked-in-http10", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let req = format!(
                "POST {} HTTP/1.0\r\nProxy-Authorization: {}\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
                env.origin.url("/echo"),
                default_auth()
            );
            c.roundtrip(req.as_bytes(), false).await?.expect(400)?;
            p.finish().await
        }),
        scenario("uploads/invalid/content-length-and-chunked", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let mut req = post_head(&env.origin.url("/echo"), "Content-Length: 5\r\nTransfer-Encoding: chunked\r\n");
            req.extend_from_slice(b"5\r\nhello\r\n0\r\n\r\n");
            c.roundtrip(&req, false).await?.expect(400)?;
            p.finish().await
        })
        .quick(),
        scenario("uploads/invalid/duplicate-content-length", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let mut req = post_head(&env.origin.url("/echo"), "Content-Length: 5\r\nContent-Length: 6\r\n");
            req.extend_from_slice(b"hello!");
            c.roundtrip(&req, false).await?.expect(400)?;
            p.finish().await
        }),
        scenario("uploads/invalid/bad-chunk-size", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let mut req = post_head(&env.origin.url("/echo"), "Transfer-Encoding: chunked\r\n");
            req.extend_from_slice(b"zz\r\nhello\r\n0\r\n\r\n");
            c.roundtrip(&req, false).await?.expect(400)?;
            drop(c);
            p.finish().await
        }),
        scenario("uploads/bytes-after-final-chunk-close", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            let mut req = post_head(&env.origin.url("/echo"), "Transfer-Encoding: chunked\r\n");
            req.extend_from_slice(b"3\r\nabc\r\n0\r\n\r\nGARBAGE");
            let r = c.roundtrip(&req, false).await?;
            r.expect(200)?;
            ensure!(split_echo(&r.body)?.1 == b"abc");
            ensure!(c.closed(Duration::from_secs(3)).await, "connection kept after trailing bytes");
            p.finish().await
        }),
    ]
}
