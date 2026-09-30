//! Plain HTTP GET workload through the proxy (absolute-form), keep-alive
//! or fresh-connection, with per-request latency capture.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinSet;

use crate::connect::{connect_proxy, h2_connect, h2_enabled, parse_url, resolve_once, ProxyStream};

/// One client connection: HTTP/1.1 byte stream, or an HTTP/2 connection
/// (requests sent one at a time, like the HTTP/1.1 keep-alive client).
enum Conn {
    H1(ProxyStream),
    H2(h2::client::SendRequest<bytes::Bytes>),
}
use crate::metrics::{pcts_ms, Metrics};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_http(
    proxy: &str,
    api_key: &str,
    url: &str,
    conns: u32,
    duration: Option<u64>,
    requests: Option<u64>,
    no_keepalive: bool,
) -> Metrics {
    let kinds: Arc<Mutex<std::collections::HashMap<String, u64>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let (host, port, path) = parse_url(url);
    let target = resolve_once(&format!("{host}:{port}"))
        .await
        .unwrap_or_else(|e| panic!("resolve {host}:{port}: {e}"));
    let (host, port, _) = parse_url(&format!("http://{target}/"));
    let counter = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let reconnects = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let lats: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));

    let start = Instant::now();
    let mut set = JoinSet::new();
    for _ in 0..conns {
        let proxy = proxy.to_string();
        let api_key = api_key.to_string();
        let host = host.clone();
        let path = path.clone();
        let counter = counter.clone();
        let errors = errors.clone();
        let reconnects = reconnects.clone();
        let bytes = bytes.clone();
        let lats = lats.clone();
        let kinds = kinds.clone();
        set.spawn(async move {
            let mut conn: Option<Conn> = None;
            loop {
                let i = counter.fetch_add(1, Ordering::Relaxed);
                // Stop on the count budget or the time budget (whichever
                // is configured). Time-budget keeps both proxies under the
                // same wall-clock protocol (3proxy needs it: its delayed-
                // ACK turned 100k 1 KB requests into ~17 min).
                if let Some(secs) = duration {
                    if start.elapsed().as_secs_f64() >= secs as f64 {
                        break;
                    }
                } else if i >= requests.unwrap_or(0) {
                    break;
                }
                if no_keepalive {
                    conn = None;
                }
                // A reused connection closed before any response byte is
                // the idle-close race every HTTP client handles by replaying
                // an idempotent request once on a fresh connection (RFC 9112
                // §9.3.1) — proxies that close silently after each response
                // (Tinyproxy) are served the same way. Counted in
                // `reconnects`; the latency includes the replay.
                let mut replayed = false;
                let mut t0: Option<Instant> = None;
                loop {
                    let reused = conn.is_some();
                    if conn.is_none() {
                        let dialed = if h2_enabled() {
                            h2_connect(&proxy).await.map(Conn::H2)
                        } else {
                            connect_proxy(&proxy).await.map(Conn::H1)
                        };
                        match dialed {
                            Ok(s) => {
                                conn = Some(s);
                            }
                            Err(_) => {
                                errors.fetch_add(1, Ordering::Relaxed);
                                *kinds.lock().unwrap().entry("connect".into()).or_insert(0) += 1;
                                break;
                            }
                        }
                    }
                    let t0 = *t0.get_or_insert_with(Instant::now);
                    let res = match conn.as_mut().unwrap() {
                        Conn::H1(stream) => {
                            http_get(stream, &host, port, &path, &api_key, !no_keepalive).await
                        }
                        Conn::H2(send) => h2_get(send, &host, port, &path, &api_key).await,
                    };
                    match res {
                        Ok((body_bytes, reusable)) => {
                            lats.lock().unwrap().push(t0.elapsed().as_secs_f64());
                            bytes.fetch_add(body_bytes, Ordering::Relaxed);
                            if !reusable {
                                // The proxy announced the close (Connection:
                                // close / HTTP/1.0): reconnect, like any client.
                                conn = None;
                            }
                        }
                        Err(e) => {
                            conn = None;
                            let msg = format!("{e:#}");
                            if reused && !replayed && msg.contains("EOF before response head") {
                                replayed = true;
                                reconnects.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            errors.fetch_add(1, Ordering::Relaxed);
                            let cls: String = if msg.contains("EOF before response head") {
                                "eof-head".into()
                            } else if msg.contains("HTTP status") {
                                // Keep the code: "bad-status-502" etc.
                                let code: String =
                                    msg.chars().filter(|c| c.is_ascii_digit()).collect();
                                format!("bad-status-{code}")
                            } else if msg.contains("EOF before body end") {
                                "eof-body".into()
                            } else if msg.contains("bad response") {
                                "bad-head".into()
                            } else {
                                "io".into()
                            };
                            *kinds.lock().unwrap().entry(cls).or_insert(0) += 1;
                        }
                    }
                    break;
                }
            }
        });
    }
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            eprintln!("task error: {e}");
        }
    }
    // Bound the guard's lifetime before the tail-expression Metrics.
    let kinds_line = kinds
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(";");
    let elapsed = start.elapsed().as_secs_f64();
    let lat = lats.lock().unwrap().clone();
    let done = lat.len() as u64;
    let (p50, p95, p99) = pcts_ms(&lat);
    Metrics {
        statics: vec![
            ("mode", "http".to_string()),
            ("err_kinds", kinds_line),
            ("url", url.to_string()),
            ("conns", conns.to_string()),
            (
                "budget",
                match (duration, requests) {
                    (Some(d), _) => format!("{d}s"),
                    (None, Some(r)) => format!("{r} reqs"),
                    _ => "?".to_string(),
                },
            ),
            ("keepalive", (!no_keepalive).to_string()),
        ],
        nums: vec![
            ("rps", done as f64 / elapsed),
            ("mbps", bytes.load(Ordering::Relaxed) as f64 / elapsed / 1e6),
            ("elapsed_s", elapsed),
            ("p50_ms", p50),
            ("p95_ms", p95),
            ("p99_ms", p99),
            ("errors", errors.load(Ordering::Relaxed) as f64),
            ("reconnects", reconnects.load(Ordering::Relaxed) as f64),
        ],
    }
}

/// One GET through the proxy. Returns the response body size on success.
async fn http_get(
    stream: &mut ProxyStream,
    host: &str,
    port: u16,
    path: &str,
    api_key: &str,
    keepalive: bool,
) -> anyhow::Result<(u64, bool)> {
    let auth = B64.encode(format!("bench:{api_key}"));
    let ka = if keepalive { "keep-alive" } else { "close" };
    let req = format!(
        "GET http://{host}:{port}{path} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Authorization: Basic {auth}\r\nConnection: {ka}\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    let mut buf = Vec::with_capacity(2048);
    let (head_len, content_length, reusable) = loop {
        buf.reserve(2048);
        let n = stream.read_buf(&mut buf).await?;
        if n == 0 {
            anyhow::bail!("EOF before response head");
        }
        let parsed = {
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut resp = httparse::Response::new(&mut headers);
            match resp.parse(&buf) {
                Ok(httparse::Status::Complete(len)) => {
                    let mut cl = None;
                    // HTTP/1.1 persists unless `close`; HTTP/1.0 only with
                    // an explicit `keep-alive`.
                    let mut reusable = resp.version == Some(1);
                    for h in resp.headers.iter() {
                        if h.name.eq_ignore_ascii_case("content-length") {
                            cl = std::str::from_utf8(h.value)
                                .ok()
                                .and_then(|s| s.trim().parse::<u64>().ok());
                        } else if h.name.eq_ignore_ascii_case("connection")
                            || h.name.eq_ignore_ascii_case("proxy-connection")
                        {
                            let v = String::from_utf8_lossy(h.value).to_ascii_lowercase();
                            if v.contains("close") {
                                reusable = false;
                            } else if v.contains("keep-alive") {
                                reusable = true;
                            }
                        }
                    }
                    Some((len, cl.unwrap_or(0), reusable && keepalive))
                }
                Ok(httparse::Status::Partial) => None,
                Err(_) => anyhow::bail!("bad response"),
            }
        };
        if let Some(v) = parsed {
            break v;
        }
        if buf.len() > 65536 {
            anyhow::bail!("response head too large");
        }
    };
    let code = std::str::from_utf8(&buf)
        .ok()
        .and_then(|s| s.lines().next())
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u32>().ok())
        .unwrap_or(0);
    if code != 200 {
        anyhow::bail!("HTTP status {code}");
    }
    let have = (buf.len() - head_len) as u64;
    let mut scratch = [0u8; 65536];
    let mut got = have;
    while got < content_length {
        let n = stream.read(&mut scratch).await?;
        if n == 0 {
            anyhow::bail!("EOF before body end");
        }
        got += n as u64;
    }
    Ok((content_length, reusable))
}

/// One GET as an HTTP/2 stream. Same result contract as [`http_get`].
async fn h2_get(
    send: &mut h2::client::SendRequest<bytes::Bytes>,
    host: &str,
    port: u16,
    path: &str,
    api_key: &str,
) -> anyhow::Result<(u64, bool)> {
    let auth = B64.encode(format!("bench:{api_key}"));
    let req = http::Request::get(format!("http://{host}:{port}{path}"))
        .header("proxy-authorization", format!("Basic {auth}"))
        .body(())?;
    let mut ready = send.clone().ready().await?;
    let (resp, _) = ready.send_request(req, true)?;
    let resp = resp.await?;
    if resp.status() != 200 {
        anyhow::bail!("HTTP status {}", resp.status().as_u16());
    }
    let mut body = resp.into_body();
    let mut got: u64 = 0;
    while let Some(chunk) = body.data().await {
        let chunk = chunk?;
        let _ = body.flow_control().release_capacity(chunk.len());
        got += chunk.len() as u64;
    }
    Ok((got, true))
}
