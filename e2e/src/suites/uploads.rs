//! Request bodies: chunked uploads (extensions, trailers, big chunks, slow
//! senders), invalid framings, bytes after the final chunk.

use std::time::Duration;
use tokio::io::AsyncWriteExt;

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
        scenario("uploads/expect-bodyless-retry", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/data/1")), false).await?.expect(200)?;
            let url = env.origin.url("/drop?key=expect-bodyless-retry");
            let request = format!("GET {url} HTTP/1.1\r\nHost: x\r\nProxy-Authorization: {}\r\nExpect: 100-continue\r\n\r\n", default_auth());
            c.roundtrip(request.as_bytes(), false).await?.expect(502)?;
            ensure!(crate::harness::origin::hits("expect-bodyless-retry") == 2);
            p.finish().await
        }).quick(),

        scenario("uploads/expect-continue-active-past-idle", |_env| async move {
            for h2 in [false, true] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let addr = listener.local_addr()?;
                let origin = tokio::spawn(async move {
                    let (socket, _) = listener.accept().await?;
                    let mut wire = crate::harness::origin::Wire::new(socket);
                    wire.head().await?.unwrap();
                    wire.s.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
                    ensure!(wire.exact(5).await? == b"hello");
                    wire.s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await?;
                    anyhow::Ok(())
                });
                let p = Proxy::start(Config::new().set("timeouts", "idle", "\"200ms\"")).await?;
                if h2 {
                    let mut send = crate::clients::h2::connect(p.tls()).await?.ready().await?;
                    let req = http::Request::builder().method("POST").uri(format!("http://{addr}/"))
                        .header("proxy-authorization", default_auth()).header("expect", "100-continue")
                        .header("content-length", "5").body(())?;
                    let (mut response, mut upload) = send.send_request(req, false)?;
                    let info = std::future::poll_fn(|cx| response.poll_informational(cx)).await.unwrap()?;
                    ensure!(info.status() == 100);
                    for (i, b) in b"hello".iter().enumerate() {
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        upload.send_data(bytes::Bytes::copy_from_slice(&[*b]), i == 4)?;
                    }
                    let r = response.await?;
                    ensure!(r.status() == 200);
                    let mut body = r.into_body();
                    let data = body.data().await.unwrap()?;
                    ensure!(data == b"ok"[..]);
                    body.flow_control().release_capacity(data.len())?;
                } else {
                    let mut c = Client::plain(p.addr).await?;
                    c.send(&post_head(&format!("http://{addr}/"), "Content-Length: 5\r\nExpect: 100-continue\r\n")).await?;
                    ensure!(c.w.head().await?.unwrap().starts_with(b"HTTP/1.1 100"));
                    for b in b"hello" {
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        c.send(&[*b]).await?;
                    }
                    c.response(false).await?.expect(200)?;
                }
                origin.await??;
                p.finish().await?;
            }
            Ok(())
        }).quick(),

        scenario("uploads/expect-continue-h2", |_env| async move {
            for immediate in [false, true] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let addr = listener.local_addr()?;
                let origin = tokio::spawn(async move {
                    let (socket, _) = listener.accept().await?;
                    let mut wire = crate::harness::origin::Wire::new(socket);
                    wire.head().await?.unwrap();
                    wire.s.write_all(b"HTTP/1.1 103 Early Hints\r\nLink: </x>\r\n\r\nHTTP/1.1 100 Continue\r\n\r\n").await?;
                    ensure!(wire.exact(5).await? == b"hello");
                    wire.s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await?;
                    anyhow::Ok(())
                });
                let p = Proxy::start(Config::new()).await?;
                let mut send = crate::clients::h2::connect(p.tls()).await?.ready().await?;
                let req = http::Request::builder().method("POST").uri(format!("http://{addr}/"))
                    .header("proxy-authorization", default_auth())
                    .header("expect", "100-continue").header("content-length", "5").body(())?;
                let (mut response, mut upload) = send.send_request(req, false)?;
                if immediate { upload.send_data(bytes::Bytes::from_static(b"hello"), true)?; }
                for status in [103, 100] {
                    let info = std::future::poll_fn(|cx| response.poll_informational(cx)).await.unwrap()?;
                    ensure!(info.status().as_u16() == status);
                }
                if !immediate { upload.send_data(bytes::Bytes::from_static(b"hello"), true)?; }
                let r = response.await?;
                ensure!(r.status() == 200);
                let mut body = r.into_body();
                let data = body.data().await.unwrap()?;
                ensure!(data == b"ok"[..]);
                body.flow_control().release_capacity(data.len())?;
                origin.await??;
                drop(send);
                p.finish().await?;
            }
            Ok(())
        }).quick(),
        scenario("uploads/expect-continue-silent-origin", |env| async move {
            let p = Proxy::start(Config::new().set("timeouts", "idle", "\"200ms\"")).await?;
            let mut c = Client::plain(p.addr).await?;
            c.send(&post_head(&env.origin.url("/hang"), "Content-Length: 5\r\nExpect: 100-continue\r\n")).await?;
            let r = c.response(false).await?;
            r.expect(504)?;
            ensure!(r.interim.is_empty(), "proxy generated interim response");
            p.finish().await
        }).quick(),
        scenario("uploads/expect-continue", |_env| async move {
            for tls in [false, true] {
                for reject in [false, true] {
                for chunked in [false, true] {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                    let addr = listener.local_addr()?;
                    let origin = tokio::spawn(async move {
                        let (socket, _) = listener.accept().await?;
                        let mut wire = crate::harness::origin::Wire::new(socket);
                        let head = wire.head().await?.unwrap();
                        ensure!(String::from_utf8_lossy(&head).to_ascii_lowercase().contains("expect: 100-continue"));
                        if reject {
                            wire.s.write_all(b"HTTP/1.1 417 Expectation Failed\r\nContent-Length: 0\r\n\r\n").await?;
                        } else {
                            wire.s.write_all(b"HTTP/1.1 103 Early Hints\r\nLink: </x>\r\n\r\nHTTP/1.1 100 Con").await?;
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            wire.s.write_all(b"tinue\r\n\r\n").await?;
                            if chunked { ensure!(wire.chunked().await?.0 == b"hello"); } else { ensure!(wire.exact(5).await? == b"hello"); }
                            wire.s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await?;
                        }
                        anyhow::Ok(())
                    });
                    let p = Proxy::start(Config::new()).await?;
                    let mut c = if tls { Client::tls(p.tls(), true).await? } else { Client::plain(p.addr).await? };
                    c.send(&post_head(&format!("http://{addr}/"), if chunked { "Transfer-Encoding: chunked\r\nExpect: 100-continue\r\n" } else { "Content-Length: 5\r\nExpect: 100-continue\r\n" })).await?;
                    if reject {
                        let response = c.response(false).await?;
                        response.expect(417)?;
                        ensure!(response.header("connection") == Some("close"));
                        ensure!(c.closed(Duration::from_secs(2)).await);
                    } else {
                        let hints = c.w.head().await?.unwrap();
                        ensure!(hints.starts_with(b"HTTP/1.1 103"));
                        let cont = c.w.head().await?.unwrap();
                        ensure!(cont.starts_with(b"HTTP/1.1 100"));
                        c.send(if chunked { b"5\r\nhello\r\n0\r\n\r\n" } else { b"hello" }).await?;
                        c.response(false).await?.expect(200)?;
                    }
                    origin.await??;
                    drop(c);
                    p.finish().await?;
                }
                }
            }
            Ok(())
        }).quick(),

        scenario("uploads/short-body-never-reuses-upstream", |_env| async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            let origin = tokio::spawn(async move {
                let (socket, _) = listener.accept().await?;
                let mut wire = crate::harness::origin::Wire::new(socket);
                wire.head().await?.unwrap();
                ensure!(wire.exact(2).await? == b"ab");
                tokio::time::sleep(Duration::from_millis(150)).await;
                wire.s.write_all(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok").await?;
                let (socket, _) = listener.accept().await?;
                let mut second = crate::harness::origin::Wire::new(socket);
                second.head().await?.unwrap();
                second.s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await?;
                anyhow::Ok(())
            });
            let cfg = Config::new().set("timeouts", "idle", "\"100ms\"");
            let p = Proxy::start(cfg).await?;
            let mut c = Client::plain(p.addr).await?;
            let mut request = post_head(&format!("http://{addr}/"), "Content-Length: 5\r\n");
            request.extend_from_slice(b"ab");
            c.send(&request).await?;
            let response = c.response(false).await?;
            response.expect(200)?;
            ensure!(response.body == b"ok");
            ensure!(response.header("connection") == Some("close"));
            ensure!(c.closed(Duration::from_secs(1)).await);
            let mut second = Client::plain(p.addr).await?;
            second.roundtrip(&get(&format!("http://{addr}/")), false).await?.expect(200)?;
            origin.await??;
            drop(c);
            drop(second);
            p.finish().await
        }).quick(),

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
