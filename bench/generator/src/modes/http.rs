//! Plain HTTP GET workload through the proxy (absolute-form), keep-alive
//! or fresh-connection, with per-request latency capture.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinSet;

use crate::connect::{parse_url, resolve_once};
use crate::metrics::{pcts_ms, Metrics};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_http(
    proxy: &str,
    api_key: &str,
    url: &str,
    conns: u32,
    requests: u64,
    no_keepalive: bool,
) -> Metrics {
    let (host, port, path) = parse_url(url);
    let target = resolve_once(&format!("{host}:{port}"))
        .await
        .unwrap_or_else(|e| panic!("resolve {host}:{port}: {e}"));
    let (host, port, _) = parse_url(&format!("http://{target}/"));
    let counter = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
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
        let bytes = bytes.clone();
        let lats = lats.clone();
        set.spawn(async move {
            let mut conn: Option<TcpStream> = None;
            loop {
                let i = counter.fetch_add(1, Ordering::Relaxed);
                if i >= requests {
                    break;
                }
                if no_keepalive {
                    conn = None;
                }
                if conn.is_none() {
                    match TcpStream::connect(&proxy).await {
                        Ok(s) => {
                            let _ = s.set_nodelay(true);
                            conn = Some(s);
                        }
                        Err(_) => {
                            errors.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
                let t0 = Instant::now();
                let stream = conn.as_mut().unwrap();
                match http_get(stream, &host, port, &path, &api_key, !no_keepalive).await {
                    Ok(body_bytes) => {
                        lats.lock().unwrap().push(t0.elapsed().as_secs_f64());
                        bytes.fetch_add(body_bytes, Ordering::Relaxed);
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        conn = None;
                    }
                }
            }
        });
    }
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            eprintln!("task error: {e}");
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let lat = lats.lock().unwrap().clone();
    let done = lat.len() as u64;
    let (p50, p95, p99) = pcts_ms(&lat);
    Metrics {
        statics: vec![
            ("mode", "http".to_string()),
            ("url", url.to_string()),
            ("conns", conns.to_string()),
            ("requests", requests.to_string()),
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
        ],
    }
}

/// One GET through the proxy. Returns the response body size on success.
async fn http_get(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    path: &str,
    api_key: &str,
    keepalive: bool,
) -> anyhow::Result<u64> {
    let auth = B64.encode(format!("bench:{api_key}"));
    let ka = if keepalive { "keep-alive" } else { "close" };
    let req = format!(
        "GET http://{host}:{port}{path} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Authorization: Basic {auth}\r\nConnection: {ka}\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;

    let mut buf = Vec::with_capacity(2048);
    let (head_len, content_length) = loop {
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
                    for h in resp.headers.iter() {
                        if h.name.eq_ignore_ascii_case("content-length") {
                            cl = std::str::from_utf8(h.value)
                                .ok()
                                .and_then(|s| s.trim().parse::<u64>().ok());
                        }
                    }
                    Some((len, cl.unwrap_or(0)))
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
    Ok(content_length)
}
