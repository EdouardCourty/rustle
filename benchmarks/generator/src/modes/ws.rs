//! WebSocket through the proxy: upgrade on the plain-HTTP path (absolute-URI
//! `GET` + `Connection: Upgrade`), then masked binary frames echoed by the
//! origin. Three shapes: ping-pong of small frames (`window` = 1: message
//! rate and round-trip latency), pipelined large frames (`window` > 1: bulk
//! throughput) and `short` (handshake + one frame + close, per iteration:
//! the cost of setting an upgraded connection up).
//!
//! The origin echoes bytes verbatim, so a frame comes back with exactly the
//! length it was sent with and the generator never parses frames.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinSet;

use crate::connect::{connect_proxy, h2_enabled, parse_url, resolve_once, ProxyStream};
use crate::metrics::{Hist, Metrics};
use crate::rt::{run_threads, share};

/// Fixed key of the RFC 6455 example; the origin answers with its accept.
const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
/// Fixed masking key: the proxy never looks at frame payloads.
const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

struct Plan {
    proxy: String,
    handshake: Vec<u8>,
    /// `window` client frames back to back (one write).
    burst: Vec<u8>,
    window: u64,
    short: bool,
}

#[derive(Default)]
struct Partial {
    hist: Option<Hist>,
    msgs: u64,
    errors: u64,
    kinds: HashMap<String, u64>,
}

impl Partial {
    fn fail(&mut self, kind: &str) {
        self.errors += 1;
        *self.kinds.entry(kind.to_string()).or_insert(0) += 1;
    }
}

/// One masked binary frame with a `size`-byte payload.
fn frame(size: usize) -> Vec<u8> {
    let mut f = Vec::with_capacity(size + 14);
    f.push(0x82); // FIN + binary
    match size {
        0..=125 => f.push(0x80 | size as u8),
        126..=65535 => {
            f.push(0x80 | 126);
            f.extend_from_slice(&(size as u16).to_be_bytes());
        }
        _ => {
            f.push(0x80 | 127);
            f.extend_from_slice(&(size as u64).to_be_bytes());
        }
    }
    f.extend_from_slice(&MASK);
    // Payload byte i = (i mod 251) XOR mask[i mod 4].
    f.extend((0..size).map(|i| ((i % 251) as u8) ^ MASK[i % 4]));
    f
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_ws(
    proxy: &str,
    api_key: &str,
    url: &str,
    conns: u32,
    duration: u64,
    size: usize,
    window: u32,
    short: bool,
    threads: usize,
) -> Metrics {
    assert!(
        !h2_enabled(),
        "ws: WebSocket upgrades are HTTP/1.1 only (no --proxy-h2)"
    );
    let http_url = url.replacen("ws://", "http://", 1);
    let (host, port, path) = parse_url(&http_url);
    let target = resolve_once(&format!("{host}:{port}"))
        .await
        .unwrap_or_else(|e| panic!("resolve {host}:{port}: {e}"));
    let auth = B64.encode(format!("bench:{api_key}"));
    let handshake = format!(
        "GET http://{target}{path} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {auth}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: {KEY}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    .into_bytes();
    let one = frame(size);
    let window = window.max(1);
    let plan = std::sync::Arc::new(Plan {
        proxy: proxy.to_string(),
        handshake,
        burst: one.repeat(window as usize),
        window: window as u64,
        short,
    });

    let start = Instant::now();
    let deadline = start + Duration::from_secs(duration);
    let parts = tokio::task::spawn_blocking(move || {
        run_threads(threads, move |ti| {
            let plan = plan.clone();
            let mine = share(conns, threads, ti);
            async move {
                let mut set = JoinSet::new();
                for _ in 0..mine {
                    set.spawn(conn_task(plan.clone(), deadline));
                }
                let mut tot = Partial {
                    hist: Some(Hist::new()),
                    ..Partial::default()
                };
                while let Some(res) = set.join_next().await {
                    match res {
                        Ok(p) => {
                            if let (Some(a), Some(b)) = (tot.hist.as_mut(), p.hist.as_ref()) {
                                a.merge(b);
                            }
                            tot.msgs += p.msgs;
                            tot.errors += p.errors;
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

    let mut hist = Hist::new();
    let (mut msgs, mut errors) = (0u64, 0u64);
    let mut kinds: HashMap<String, u64> = HashMap::new();
    for p in parts {
        if let Some(h) = p.hist.as_ref() {
            hist.merge(h);
        }
        msgs += p.msgs;
        errors += p.errors;
        for (k, v) in p.kinds {
            *kinds.entry(k).or_insert(0) += v;
        }
    }
    let mut kinds: Vec<_> = kinds.into_iter().collect();
    kinds.sort();
    let kinds_line = kinds
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(";");
    let (p50, p95, p99) = hist.pcts_ms();
    Metrics {
        statics: vec![
            ("mode", "ws".to_string()),
            ("err_kinds", kinds_line),
            ("url", url.to_string()),
            ("conns", conns.to_string()),
            ("size", size.to_string()),
            ("window", window.to_string()),
            ("short", short.to_string()),
            ("budget", format!("{duration}s")),
        ],
        nums: vec![
            // Messages per second (frames echoed, not bursts).
            ("rps", msgs as f64 / elapsed),
            // Payload MB/s in one direction (the echo doubles it on the wire).
            ("mbps", (msgs * size as u64) as f64 / elapsed / 1e6),
            ("elapsed_s", elapsed),
            ("p50_ms", p50),
            ("p95_ms", p95),
            ("p99_ms", p99),
            ("errors", errors as f64),
        ],
    }
}

/// Dials the proxy and upgrades; the connection is then a raw byte stream.
async fn open(plan: &Plan, scratch: &mut [u8]) -> Result<ProxyStream, String> {
    let mut s = connect_proxy(&plan.proxy)
        .await
        .map_err(|_| "connect".to_string())?;
    s.write_all(&plan.handshake)
        .await
        .map_err(|_| "io".to_string())?;
    s.flush().await.map_err(|_| "io".to_string())?;
    let mut got = 0;
    loop {
        let n = s
            .read(&mut scratch[got..])
            .await
            .map_err(|_| "io".to_string())?;
        if n == 0 {
            return Err("eof-head".into());
        }
        got += n;
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut resp = httparse::Response::new(&mut headers);
        match resp.parse(&scratch[..got]) {
            Ok(httparse::Status::Complete(_)) => {
                let code = resp.code.unwrap_or(0);
                return if code == 101 {
                    Ok(s)
                } else {
                    Err(format!("status-{code}"))
                };
            }
            Ok(httparse::Status::Partial) if got < scratch.len() => {}
            _ => return Err("bad-head".into()),
        }
    }
}

/// Writes one burst and reads back as many bytes.
async fn roundtrip(s: &mut ProxyStream, burst: &[u8], scratch: &mut [u8]) -> std::io::Result<()> {
    s.write_all(burst).await?;
    s.flush().await?;
    let mut left = burst.len();
    let cap = scratch.len();
    while left > 0 {
        let n = s.read(&mut scratch[..left.min(cap)]).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        left -= n;
    }
    Ok(())
}

async fn conn_task(plan: std::sync::Arc<Plan>, deadline: Instant) -> Partial {
    let mut p = Partial {
        hist: Some(Hist::new()),
        ..Partial::default()
    };
    let mut scratch = vec![0u8; 256 * 1024];
    let mut stream: Option<ProxyStream> = None;
    while Instant::now() < deadline {
        let t0 = Instant::now();
        if stream.is_none() {
            match open(&plan, &mut scratch).await {
                Ok(s) => stream = Some(s),
                Err(kind) => {
                    p.fail(&kind);
                    // A refusing proxy answers instantly: do not spin.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            }
        }
        let s = stream.as_mut().expect("connected");
        match roundtrip(s, &plan.burst, &mut scratch).await {
            Ok(()) => {
                if let Some(h) = p.hist.as_mut() {
                    h.record(t0.elapsed());
                }
                p.msgs += plan.window;
                if plan.short {
                    stream = None;
                }
            }
            Err(_) => {
                p.fail("io");
                stream = None;
            }
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_headers_by_size() {
        assert_eq!(&frame(64)[..2], &[0x82, 0x80 | 64]);
        assert_eq!(frame(64).len(), 2 + 4 + 64);
        assert_eq!(&frame(1000)[..4], &[0x82, 0xfe, 0x03, 0xe8]);
        assert_eq!(frame(1000).len(), 4 + 4 + 1000);
        assert_eq!(&frame(70_000)[..2], &[0x82, 0xff]);
        assert_eq!(frame(70_000).len(), 10 + 4 + 70_000);
    }
}
