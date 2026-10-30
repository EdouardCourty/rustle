//! Plain HTTP GET workload through the proxy (absolute-form), keep-alive
//! or fresh-connection, with per-request latency capture.
//!
//! Harness cost is the ceiling of every measurement, so the hot path is
//! built like a proxy's: shared-nothing threads (one `current_thread`
//! runtime each, connections split between them), request bytes built once,
//! one reusable buffer per connection, allocation-free response parsing,
//! per-thread counters and latency histograms merged at the end (no shared
//! atomic or lock is touched per request).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinSet;

use crate::connect::{connect_proxy, h2_connect, h2_enabled, parse_url, resolve_once, ProxyStream};
use crate::metrics::{Hist, Metrics};
use crate::rt::{run_threads, share};

/// One client connection: HTTP/1.1 byte stream, or an HTTP/2 connection
/// (requests sent one at a time, like the HTTP/1.1 keep-alive client).
enum Conn {
    H1(ProxyStream),
    H2(h2::client::SendRequest<bytes::Bytes>),
}

/// Everything a request needs, built once per run.
struct Plan {
    proxy: String,
    /// GET head, `Connection: keep-alive` / `Connection: close`.
    req_ka: Vec<u8>,
    req_close: Vec<u8>,
    /// h2: absolute URI + Proxy-Authorization value.
    uri: String,
    auth: String,
    /// Body size the benchmark origin serves at this path (`/1kb` .. `/10mb`):
    /// any other length is a failure, not a success with fewer bytes.
    expect: Option<u64>,
}

/// One thread's share of the results.
struct Partial {
    hist: Hist,
    bytes: u64,
    errors: u64,
    reconnects: u64,
    kinds: HashMap<String, u64>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_http(
    proxy: &str,
    api_key: &str,
    url: &str,
    conns: u32,
    duration: Option<u64>,
    requests: Option<u64>,
    no_keepalive: bool,
    threads: usize,
) -> Metrics {
    let (host, port, path) = parse_url(url);
    let target = resolve_once(&format!("{host}:{port}"))
        .await
        .unwrap_or_else(|e| panic!("resolve {host}:{port}: {e}"));
    let (host, port, _) = parse_url(&format!("http://{target}/"));
    let auth = B64.encode(format!("bench:{api_key}"));
    let head = |ka: &str| {
        format!(
            "GET http://{host}:{port}{path} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Authorization: Basic {auth}\r\nConnection: {ka}\r\n\r\n"
        )
        .into_bytes()
    };
    let plan = std::sync::Arc::new(Plan {
        proxy: proxy.to_string(),
        req_ka: head("keep-alive"),
        req_close: head("close"),
        uri: format!("http://{host}:{port}{path}"),
        auth: format!("Basic {auth}"),
        expect: match path.as_str() {
            "/1kb" => Some(1024),
            "/10kb" => Some(10 * 1024),
            "/1mb" => Some(1024 * 1024),
            "/10mb" => Some(10 * 1024 * 1024),
            _ => None,
        },
    });

    let start = Instant::now();
    let deadline = duration.map(|d| start + Duration::from_secs(d));
    // Count budget: split evenly over the connections.
    let per_conn = requests.map(|r| r / conns.max(1) as u64);
    let h2 = h2_enabled();
    let parts = tokio::task::spawn_blocking(move || {
        run_threads(threads, move |ti| {
            let plan = plan.clone();
            let mine = share(conns, threads, ti);
            async move {
                let mut set = JoinSet::new();
                for _ in 0..mine {
                    let plan = plan.clone();
                    set.spawn(conn_task(plan, deadline, per_conn, no_keepalive, h2));
                }
                let mut tot = Partial {
                    hist: Hist::new(),
                    bytes: 0,
                    errors: 0,
                    reconnects: 0,
                    kinds: HashMap::new(),
                };
                while let Some(res) = set.join_next().await {
                    match res {
                        Ok(p) => {
                            tot.hist.merge(&p.hist);
                            tot.bytes += p.bytes;
                            tot.errors += p.errors;
                            tot.reconnects += p.reconnects;
                            for (k, v) in p.kinds {
                                *tot.kinds.entry(k).or_insert(0) += v;
                            }
                        }
                        Err(e) => eprintln!("task error: {e}"),
                    }
                }
                tot
            }
        })
    })
    .await
    .expect("worker threads");
    let elapsed = start.elapsed().as_secs_f64();

    let mut all = Partial {
        hist: Hist::new(),
        bytes: 0,
        errors: 0,
        reconnects: 0,
        kinds: HashMap::new(),
    };
    for p in parts {
        all.hist.merge(&p.hist);
        all.bytes += p.bytes;
        all.errors += p.errors;
        all.reconnects += p.reconnects;
        for (k, v) in p.kinds {
            *all.kinds.entry(k).or_insert(0) += v;
        }
    }
    let mut kinds: Vec<_> = all.kinds.iter().collect();
    kinds.sort();
    let kinds_line = kinds
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(";");
    let done = all.hist.len();
    let (p50, p95, p99) = all.hist.pcts_ms();
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
            ("mbps", all.bytes as f64 / elapsed / 1e6),
            ("elapsed_s", elapsed),
            ("p50_ms", p50),
            ("p95_ms", p95),
            ("p99_ms", p99),
            ("errors", all.errors as f64),
            ("reconnects", all.reconnects as f64),
        ],
    }
}

/// One connection's request loop (until the deadline or its request quota).
async fn conn_task(
    plan: std::sync::Arc<Plan>,
    deadline: Option<Instant>,
    quota: Option<u64>,
    no_keepalive: bool,
    h2: bool,
) -> Partial {
    let mut p = Partial {
        hist: Hist::new(),
        bytes: 0,
        errors: 0,
        reconnects: 0,
        kinds: HashMap::new(),
    };
    // Reused for every response of this connection (head + body drain).
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut conn: Option<Conn> = None;
    let mut issued: u64 = 0;
    loop {
        match (deadline, quota) {
            (Some(d), _) => {
                if Instant::now() >= d {
                    break;
                }
            }
            (None, Some(q)) => {
                if issued >= q {
                    break;
                }
                issued += 1;
            }
            (None, None) => break,
        }
        if no_keepalive {
            conn = None;
        }
        // A reused connection closed before any response byte is the
        // idle-close race every HTTP client handles by replaying an
        // idempotent request once on a fresh connection (RFC 9112 §9.3.1) —
        // proxies that close silently after each response (Tinyproxy) are
        // served the same way. Counted in `reconnects`; the latency
        // includes the replay.
        let mut replayed = false;
        let t0 = Instant::now();
        loop {
            let reused = conn.is_some();
            if conn.is_none() {
                let dialed = if h2 {
                    h2_connect(&plan.proxy).await.map(Conn::H2)
                } else {
                    connect_proxy(&plan.proxy).await.map(Conn::H1)
                };
                match dialed {
                    Ok(s) => conn = Some(s),
                    Err(_) => {
                        p.errors += 1;
                        *p.kinds.entry("connect".into()).or_insert(0) += 1;
                        break;
                    }
                }
            }
            let res = match conn.as_mut().unwrap() {
                Conn::H1(stream) => {
                    let req = if no_keepalive {
                        &plan.req_close
                    } else {
                        &plan.req_ka
                    };
                    http_get(stream, req, !no_keepalive, &mut buf, plan.expect).await
                }
                Conn::H2(send) => h2_get(send, &plan.uri, &plan.auth, plan.expect).await,
            };
            match res {
                Ok((body_bytes, reusable)) => {
                    p.hist.record(t0.elapsed());
                    p.bytes += body_bytes;
                    if !reusable {
                        // The proxy announced the close (Connection: close /
                        // HTTP/1.0): reconnect, like any client.
                        conn = None;
                    }
                }
                Err(e) => {
                    conn = None;
                    let msg = format!("{e:#}");
                    if reused && !replayed && msg.contains("EOF before response head") {
                        replayed = true;
                        p.reconnects += 1;
                        continue;
                    }
                    p.errors += 1;
                    let cls: String = if msg.contains("EOF before response head") {
                        "eof-head".into()
                    } else if msg.contains("HTTP status") {
                        // Keep the code: "bad-status-502" etc.
                        let code: String = msg.chars().filter(|c| c.is_ascii_digit()).collect();
                        format!("bad-status-{code}")
                    } else if msg.contains("EOF before body end") {
                        "eof-body".into()
                    } else if msg.contains("bad response") {
                        "bad-head".into()
                    } else {
                        "io".into()
                    };
                    *p.kinds.entry(cls).or_insert(0) += 1;
                }
            }
            break;
        }
    }
    p
}

/// Case-insensitive substring test on header bytes (no allocation).
fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

/// One GET through the proxy. Returns the response body size on success.
async fn http_get(
    stream: &mut ProxyStream,
    req: &[u8],
    keepalive: bool,
    buf: &mut Vec<u8>,
    expect: Option<u64>,
) -> anyhow::Result<(u64, bool)> {
    stream.write_all(req).await?;
    stream.flush().await?;

    buf.clear();
    let (head_len, content_length, reusable) = loop {
        buf.reserve(2048);
        let n = stream.read_buf(buf).await?;
        if n == 0 {
            anyhow::bail!("EOF before response head");
        }
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut resp = httparse::Response::new(&mut headers);
        match resp.parse(buf) {
            Ok(httparse::Status::Complete(len)) => {
                let code = resp.code.unwrap_or(0);
                if code != 200 {
                    anyhow::bail!("HTTP status {code}");
                }
                let mut cl = None;
                // HTTP/1.1 persists unless `close`; HTTP/1.0 only with an
                // explicit `keep-alive`.
                let mut reusable = resp.version == Some(1);
                for h in resp.headers.iter() {
                    if h.name.eq_ignore_ascii_case("content-length") {
                        cl = std::str::from_utf8(h.value)
                            .ok()
                            .and_then(|s| s.trim().parse::<u64>().ok());
                    } else if h.name.eq_ignore_ascii_case("connection")
                        || h.name.eq_ignore_ascii_case("proxy-connection")
                    {
                        if contains_ci(h.value, b"close") {
                            reusable = false;
                        } else if contains_ci(h.value, b"keep-alive") {
                            reusable = true;
                        }
                    }
                }
                if let Some(want) = expect {
                    if cl != Some(want) {
                        anyhow::bail!("bad response: body length {cl:?}, expected {want}");
                    }
                }
                break (len, cl.unwrap_or(0), reusable && keepalive);
            }
            Ok(httparse::Status::Partial) => {}
            Err(_) => anyhow::bail!("bad response"),
        }
        if buf.len() > 65536 {
            anyhow::bail!("response head too large");
        }
    };
    let mut got = (buf.len() - head_len) as u64;
    while got < content_length {
        // The reused buffer doubles as the drain buffer: nothing in it is
        // needed any more, and no 64 KB scratch is zeroed per request.
        buf.clear();
        let n = stream.read_buf(buf).await?;
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
    uri: &str,
    auth: &str,
    expect: Option<u64>,
) -> anyhow::Result<(u64, bool)> {
    let req = http::Request::get(uri)
        .header("proxy-authorization", auth)
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
    if let Some(want) = expect {
        if got != want {
            anyhow::bail!("bad response: body length {got}, expected {want}");
        }
    }
    Ok((got, true))
}
