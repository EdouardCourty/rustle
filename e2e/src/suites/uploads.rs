//! Request bodies: chunked uploads (extensions, trailers, big chunks, slow
//! senders), invalid framings, bytes after the final chunk.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use anyhow::ensure;

use super::matrix::split_echo;
use crate::clients::http1::{self, default_auth, get, Client};
use crate::harness::origin::Wire;
use crate::harness::{payload, Config, Env, Proxy};
use crate::runner::{scenario, Scenario};

fn post_head(url: &str, extra: &str) -> Vec<u8> {
    format!(
        "POST {url} HTTP/1.1\r\nHost: x\r\nProxy-Authorization: {}\r\n{extra}\r\n",
        default_auth()
    )
    .into_bytes()
}

const OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";
const HINTS_AND_CONTINUE: &[u8] =
    b"HTTP/1.1 103 Early Hints\r\nLink: </x>\r\n\r\nHTTP/1.1 100 Continue\r\n\r\n";

async fn bind_origin() -> anyhow::Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    Ok((listener, addr))
}

async fn accept_request(listener: &TcpListener) -> anyhow::Result<Wire<TcpStream>> {
    let (socket, _) = listener.accept().await?;
    let mut wire = Wire::new(socket);
    ensure!(
        wire.head().await?.is_some(),
        "origin received no request head"
    );
    Ok(wire)
}

async fn serve_hello_upload(
    listener: TcpListener,
    informational: &'static [u8],
) -> anyhow::Result<()> {
    let mut wire = accept_request(&listener).await?;
    wire.s.write_all(informational).await?;
    UploadFraming::ContentLength.expect_hello(&mut wire).await?;
    wire.s.write_all(OK_RESPONSE).await?;
    Ok(())
}

fn h2_expect_request(addr: SocketAddr) -> anyhow::Result<http::Request<()>> {
    Ok(http::Request::builder()
        .method("POST")
        .uri(format!("http://{addr}/"))
        .header("proxy-authorization", default_auth())
        .header("expect", "100-continue")
        .header("content-length", "5")
        .body(())?)
}

async fn expect_h2_information(
    response: &mut h2::client::ResponseFuture,
    status: u16,
) -> anyhow::Result<()> {
    let info = std::future::poll_fn(|cx| response.poll_informational(cx))
        .await
        .ok_or_else(|| anyhow::anyhow!("missing informational response {status}"))??;
    ensure!(info.status().as_u16() == status);
    Ok(())
}

async fn expect_h2_ok(response: h2::client::ResponseFuture) -> anyhow::Result<()> {
    let response = response.await?;
    ensure!(response.status() == 200);
    let mut body = response.into_body();
    let data = body.data().await.unwrap()?;
    ensure!(data == b"ok"[..]);
    body.flow_control().release_capacity(data.len())?;
    Ok(())
}

async fn expect_http1_information(client: &mut Client, status: u16) -> anyhow::Result<()> {
    let head = client.w.head().await?.unwrap();
    ensure!(head.starts_with(format!("HTTP/1.1 {status}").as_bytes()));
    Ok(())
}

#[derive(Clone, Copy)]
enum UploadFraming {
    ContentLength,
    Chunked,
}

impl UploadFraming {
    fn expect_headers(self) -> &'static str {
        match self {
            Self::ContentLength => "Content-Length: 5\r\nExpect: 100-continue\r\n",
            Self::Chunked => "Transfer-Encoding: chunked\r\nExpect: 100-continue\r\n",
        }
    }

    fn hello_body(self) -> &'static [u8] {
        match self {
            Self::ContentLength => b"hello",
            Self::Chunked => b"5\r\nhello\r\n0\r\n\r\n",
        }
    }

    async fn expect_hello(self, wire: &mut Wire<TcpStream>) -> anyhow::Result<()> {
        let body = match self {
            Self::ContentLength => wire.exact(5).await?,
            Self::Chunked => wire.chunked().await?.0,
        };
        ensure!(body == b"hello");
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Http1Transport {
    Plain,
    Tls,
}

impl Http1Transport {
    async fn connect(self, proxy: &Proxy) -> anyhow::Result<Client> {
        match self {
            Self::Plain => Client::plain(proxy.addr).await,
            Self::Tls => Client::tls(proxy.tls(), true).await,
        }
    }
}

#[derive(Clone, Copy)]
enum OriginDecision {
    Continue,
    Reject,
}

async fn serve_expect_request(
    listener: TcpListener,
    decision: OriginDecision,
    framing: UploadFraming,
) -> anyhow::Result<()> {
    let (socket, _) = listener.accept().await?;
    let mut wire = Wire::new(socket);
    let head = wire.head().await?.unwrap();
    ensure!(String::from_utf8_lossy(&head)
        .to_ascii_lowercase()
        .contains("expect: 100-continue"));
    match decision {
        OriginDecision::Reject => {
            wire.s
                .write_all(b"HTTP/1.1 417 Expectation Failed\r\nContent-Length: 0\r\n\r\n")
                .await?;
        }
        OriginDecision::Continue => {
            wire.s
                .write_all(b"HTTP/1.1 103 Early Hints\r\nLink: </x>\r\n\r\nHTTP/1.1 100 Con")
                .await?;
            tokio::time::sleep(Duration::from_millis(10)).await;
            wire.s.write_all(b"tinue\r\n\r\n").await?;
            framing.expect_hello(&mut wire).await?;
            wire.s.write_all(OK_RESPONSE).await?;
        }
    }
    Ok(())
}

async fn run_http1_expect_case(
    transport: Http1Transport,
    decision: OriginDecision,
    framing: UploadFraming,
) -> anyhow::Result<()> {
    let (listener, addr) = bind_origin().await?;
    let origin = tokio::spawn(serve_expect_request(listener, decision, framing));
    let proxy = Proxy::start(Config::new()).await?;
    let mut client = transport.connect(&proxy).await?;
    client
        .send(&post_head(
            &format!("http://{addr}/"),
            framing.expect_headers(),
        ))
        .await?;
    match decision {
        OriginDecision::Reject => {
            let response = client.response(false).await?;
            response.expect(417)?;
            ensure!(response.header("connection") == Some("close"));
            ensure!(client.closed(Duration::from_secs(2)).await);
        }
        OriginDecision::Continue => {
            expect_http1_information(&mut client, 103).await?;
            expect_http1_information(&mut client, 100).await?;
            client.send(framing.hello_body()).await?;
            client.response(false).await?.expect(200)?;
        }
    }
    origin.await??;
    drop(client);
    proxy.finish().await
}

pub fn scenarios() -> Vec<Scenario> {
    vec![
        scenario("uploads/expect-bodyless-retry", expect_bodyless_retry).quick(),
        scenario(
            "uploads/expect-continue-active-past-idle",
            expect_continue_active_past_idle,
        )
        .quick(),
        scenario("uploads/expect-continue-h2", expect_continue_h2).quick(),
        scenario(
            "uploads/expect-continue-silent-origin",
            expect_continue_silent_origin,
        )
        .quick(),
        scenario("uploads/expect-continue", expect_continue).quick(),
        scenario(
            "uploads/short-body-never-reuses-upstream",
            short_body_never_reuses_upstream,
        )
        .quick(),
        scenario(
            "uploads/chunked-extensions-and-trailers",
            chunked_extensions_and_trailers,
        )
        .quick(),
        scenario("uploads/chunked-one-huge-chunk", chunked_one_huge_chunk),
        scenario(
            "uploads/chunked-uppercase-hex-and-tiny-chunks",
            chunked_uppercase_hex_and_tiny_chunks,
        ),
        scenario("uploads/slow-sender", slow_sender),
        scenario(
            "uploads/invalid/chunked-in-http10",
            invalid_chunked_in_http10,
        ),
        scenario(
            "uploads/invalid/content-length-and-chunked",
            invalid_content_length_and_chunked,
        )
        .quick(),
        scenario(
            "uploads/invalid/duplicate-content-length",
            invalid_duplicate_content_length,
        ),
        scenario("uploads/invalid/bad-chunk-size", invalid_bad_chunk_size),
        scenario(
            "uploads/bytes-after-final-chunk-close",
            bytes_after_final_chunk_close,
        ),
    ]
}

async fn expect_bodyless_retry(env: Arc<Env>) -> anyhow::Result<()> {
    let p = Proxy::start(Config::new()).await?;
    let mut c = Client::plain(p.addr).await?;
    c.roundtrip(&get(&env.origin.url("/data/1")), false)
        .await?
        .expect(200)?;
    let url = env.origin.url("/drop?key=expect-bodyless-retry");
    let request = format!(
        "GET {url} HTTP/1.1\r\nHost: x\r\nProxy-Authorization: {}\r\nExpect: 100-continue\r\n\r\n",
        default_auth()
    );
    c.roundtrip(request.as_bytes(), false).await?.expect(502)?;
    ensure!(crate::harness::origin::hits("expect-bodyless-retry") == 2);
    p.finish().await
}

async fn expect_continue_active_past_idle(_env: Arc<Env>) -> anyhow::Result<()> {
    for h2 in [false, true] {
        let (listener, addr) = bind_origin().await?;
        let origin = tokio::spawn(serve_hello_upload(listener, CONTINUE));
        let p = Proxy::start(Config::new().set("timeouts", "idle", "\"200ms\"")).await?;
        if h2 {
            let mut send = crate::clients::h2::connect(p.tls()).await?.ready().await?;
            let req = h2_expect_request(addr)?;
            let (mut response, mut upload) = send.send_request(req, false)?;
            expect_h2_information(&mut response, 100).await?;
            for (i, b) in b"hello".iter().enumerate() {
                tokio::time::sleep(Duration::from_millis(80)).await;
                upload.send_data(bytes::Bytes::copy_from_slice(&[*b]), i == 4)?;
            }
            expect_h2_ok(response).await?;
        } else {
            let mut c = Client::plain(p.addr).await?;
            c.send(&post_head(
                &format!("http://{addr}/"),
                "Content-Length: 5\r\nExpect: 100-continue\r\n",
            ))
            .await?;
            expect_http1_information(&mut c, 100).await?;
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
}

async fn expect_continue_h2(_env: Arc<Env>) -> anyhow::Result<()> {
    for immediate in [false, true] {
        let (listener, addr) = bind_origin().await?;
        let origin = tokio::spawn(serve_hello_upload(listener, HINTS_AND_CONTINUE));
        let p = Proxy::start(Config::new()).await?;
        let mut send = crate::clients::h2::connect(p.tls()).await?.ready().await?;
        let req = h2_expect_request(addr)?;
        let (mut response, mut upload) = send.send_request(req, false)?;
        if immediate {
            upload.send_data(bytes::Bytes::from_static(b"hello"), true)?;
        }
        for status in [103, 100] {
            expect_h2_information(&mut response, status).await?;
        }
        if !immediate {
            upload.send_data(bytes::Bytes::from_static(b"hello"), true)?;
        }
        expect_h2_ok(response).await?;
        origin.await??;
        drop(send);
        p.finish().await?;
    }
    Ok(())
}

async fn expect_continue_silent_origin(env: Arc<Env>) -> anyhow::Result<()> {
    let p = Proxy::start(Config::new().set("timeouts", "idle", "\"200ms\"")).await?;
    let mut c = Client::plain(p.addr).await?;
    c.send(&post_head(
        &env.origin.url("/hang"),
        "Content-Length: 5\r\nExpect: 100-continue\r\n",
    ))
    .await?;
    let r = c.response(false).await?;
    r.expect(504)?;
    ensure!(r.interim.is_empty(), "proxy generated interim response");
    p.finish().await
}

async fn expect_continue(_env: Arc<Env>) -> anyhow::Result<()> {
    for transport in [Http1Transport::Plain, Http1Transport::Tls] {
        for decision in [OriginDecision::Continue, OriginDecision::Reject] {
            for framing in [UploadFraming::ContentLength, UploadFraming::Chunked] {
                run_http1_expect_case(transport, decision, framing).await?;
            }
        }
    }
    Ok(())
}

async fn short_body_never_reuses_upstream(_env: Arc<Env>) -> anyhow::Result<()> {
    let (listener, addr) = bind_origin().await?;
    let origin = tokio::spawn(async move {
        let mut wire = accept_request(&listener).await?;
        ensure!(wire.exact(2).await? == b"ab");
        tokio::time::sleep(Duration::from_millis(150)).await;
        wire.s
            .write_all(
                concat!(
                    "HTTP/1.1 100 Continue\r\n\r\n",
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                )
                .as_bytes(),
            )
            .await?;
        let mut second = accept_request(&listener).await?;
        second.s.write_all(OK_RESPONSE).await?;
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
    second
        .roundtrip(&get(&format!("http://{addr}/")), false)
        .await?
        .expect(200)?;
    origin.await??;
    drop(c);
    drop(second);
    p.finish().await
}

async fn chunked_extensions_and_trailers(env: Arc<Env>) -> anyhow::Result<()> {
    let p = Proxy::start(Config::new()).await?;
    let mut c = Client::plain(p.addr).await?;
    let mut req = post_head(&env.origin.url("/echo"), "Transfer-Encoding: chunked\r\n");
    req.extend_from_slice(
        b"5;name=value\r\nhello\r\n6;x\r\n world\r\n0\r\nX-Trailer: t\r\nX-Other: o\r\n\r\n",
    );
    let r = c.roundtrip(&req, false).await?;
    r.expect(200)?;
    let (_, body) = split_echo(&r.body)?;
    ensure!(
        body == b"hello world",
        "{:?}",
        String::from_utf8_lossy(body)
    );
    c.roundtrip(&get(&env.origin.url("/data/10")), false)
        .await?
        .expect(200)?;
    p.finish().await
}

async fn chunked_one_huge_chunk(env: Arc<Env>) -> anyhow::Result<()> {
    let p = Proxy::start(Config::new()).await?;
    let mut c = Client::plain(p.addr).await?;
    let body = payload::make(42, 12 << 20);
    let mut req = post_head(&env.origin.url("/echo"), "Transfer-Encoding: chunked\r\n");
    req.extend(http1::chunked(&body, body.len(), None));
    let r = c.roundtrip(&req, false).await?;
    r.expect(200)?;
    payload::check(42, 12 << 20, split_echo(&r.body)?.1)?;
    p.finish().await
}

async fn chunked_uppercase_hex_and_tiny_chunks(env: Arc<Env>) -> anyhow::Result<()> {
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
}

async fn slow_sender(env: Arc<Env>) -> anyhow::Result<()> {
    let p = Proxy::start(Config::new()).await?;
    let mut c = Client::plain(p.addr).await?;
    let body = payload::make(8, 100_000);
    c.send(&post_head(
        &env.origin.url("/echo"),
        &format!("Content-Length: {}\r\n", body.len()),
    ))
    .await?;
    for piece in body.chunks(10_000) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        c.send(piece).await?;
    }
    let r = c.response(false).await?;
    r.expect(200)?;
    payload::check(8, 100_000, split_echo(&r.body)?.1)?;
    p.finish().await
}

async fn invalid_chunked_in_http10(env: Arc<Env>) -> anyhow::Result<()> {
    let p = Proxy::start(Config::new()).await?;
    let mut c = Client::plain(p.addr).await?;
    let req = format!(
        "POST {} HTTP/1.0\r\nProxy-Authorization: {}\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        env.origin.url("/echo"),
        default_auth()
    );
    c.roundtrip(req.as_bytes(), false).await?.expect(400)?;
    p.finish().await
}

async fn invalid_content_length_and_chunked(env: Arc<Env>) -> anyhow::Result<()> {
    let p = Proxy::start(Config::new()).await?;
    let mut c = Client::plain(p.addr).await?;
    let mut req = post_head(
        &env.origin.url("/echo"),
        "Content-Length: 5\r\nTransfer-Encoding: chunked\r\n",
    );
    req.extend_from_slice(b"5\r\nhello\r\n0\r\n\r\n");
    c.roundtrip(&req, false).await?.expect(400)?;
    p.finish().await
}

async fn invalid_duplicate_content_length(env: Arc<Env>) -> anyhow::Result<()> {
    let p = Proxy::start(Config::new()).await?;
    let mut c = Client::plain(p.addr).await?;
    let mut req = post_head(
        &env.origin.url("/echo"),
        "Content-Length: 5\r\nContent-Length: 6\r\n",
    );
    req.extend_from_slice(b"hello!");
    c.roundtrip(&req, false).await?.expect(400)?;
    p.finish().await
}

async fn invalid_bad_chunk_size(env: Arc<Env>) -> anyhow::Result<()> {
    let p = Proxy::start(Config::new()).await?;
    let mut c = Client::plain(p.addr).await?;
    let mut req = post_head(&env.origin.url("/echo"), "Transfer-Encoding: chunked\r\n");
    req.extend_from_slice(b"zz\r\nhello\r\n0\r\n\r\n");
    c.roundtrip(&req, false).await?.expect(400)?;
    drop(c);
    p.finish().await
}

async fn bytes_after_final_chunk_close(env: Arc<Env>) -> anyhow::Result<()> {
    let p = Proxy::start(Config::new()).await?;
    let mut c = Client::plain(p.addr).await?;
    let mut req = post_head(&env.origin.url("/echo"), "Transfer-Encoding: chunked\r\n");
    req.extend_from_slice(b"3\r\nabc\r\n0\r\n\r\nGARBAGE");
    let r = c.roundtrip(&req, false).await?;
    r.expect(200)?;
    ensure!(split_echo(&r.body)?.1 == b"abc");
    ensure!(
        c.closed(Duration::from_secs(3)).await,
        "connection kept after trailing bytes"
    );
    p.finish().await
}
