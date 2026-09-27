use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(Parser)]
#[command(name = "bench-client", about = "Load generator for the proxy benchmark")]
struct Cli {
    /// Proxy address host:port.
    #[arg(long, default_value = "proxy:8080")]
    proxy: String,
    /// API key sent as Basic auth password (user is always "bench").
    #[arg(long, default_value = "secret1")]
    api_key: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Plain HTTP GET requests through the proxy (absolute-form).
    Http {
        /// Target URL, e.g. http://upstream:8000/1kb
        url: String,
        /// Concurrent connections.
        #[arg(long)]
        conns: u32,
        /// Total number of requests.
        #[arg(long)]
        requests: u64,
        /// Open a fresh connection for every request.
        #[arg(long)]
        no_keepalive: bool,
    },
    /// CONNECT tunnels with bulk upload; measures Gbps through the tunnel.
    Flood {
        /// Target host:port (TCP sink).
        target: String,
        /// Concurrent tunnels.
        #[arg(long)]
        conns: u32,
        /// Duration in seconds.
        #[arg(long, default_value_t = 10)]
        duration: u64,
        /// Write chunk size in bytes.
        #[arg(long, default_value_t = 65536)]
        chunk: usize,
    },
    /// Open N CONNECT tunnels concurrently and hold them.
    Hold {
        /// Target host:port (TCP sink).
        target: String,
        /// Total tunnels to open.
        #[arg(long)]
        conns: u32,
        /// Hold duration in seconds.
        #[arg(long, default_value_t = 10)]
        duration: u64,
        /// Concurrent connect attempts (ramp).
        #[arg(long, default_value_t = 4096)]
        ramp: u32,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Http {
            url,
            conns,
            requests,
            no_keepalive,
        } => run_http(&cli.proxy, &cli.api_key, &url, conns, requests, no_keepalive).await,
        Cmd::Flood {
            target,
            conns,
            duration,
            chunk,
        } => run_flood(&cli.proxy, &cli.api_key, &target, conns, duration, chunk).await,
        Cmd::Hold {
            target,
            conns,
            duration,
            ramp,
        } => run_hold(&cli.proxy, &cli.api_key, &target, conns, duration, ramp).await,
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn print_latency(label: &str, mut v: Vec<f64>) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    if n == 0 {
        println!("{label}: n/a");
        return;
    }
    let sum: f64 = v.iter().sum();
    println!(
        "{label}: n={n} avg={:.3}ms p50={:.3}ms p95={:.3}ms p99={:.3}ms",
        sum / n as f64 * 1e3,
        pct(&v, 0.50) * 1e3,
        pct(&v, 0.95) * 1e3,
        pct(&v, 0.99) * 1e3,
    );
}

fn parse_url(url: &str) -> (String, u16, String) {
    let rest = url
        .strip_prefix("http://")
        .expect("url must start with http://");
    let slash = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..slash];
    let path = if slash == rest.len() {
        "/".to_string()
    } else {
        rest[slash..].to_string()
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => (authority[..i].to_string(), authority[i + 1..].parse().unwrap()),
        None => (authority.to_string(), 80),
    };
    (host, port, path)
}

/// TCP connect to the proxy, send CONNECT, wait for the 200.
async fn connect_via_proxy(proxy: &str, target: &str, api_key: &str) -> anyhow::Result<TcpStream> {
    let mut s = TcpStream::connect(proxy).await?;
    s.set_nodelay(true)?;
    let auth = B64.encode(format!("bench:{api_key}"));
    let req = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {auth}\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await?;
    let mut buf = Vec::with_capacity(256);
    loop {
        buf.reserve(512);
        let n = s.read_buf(&mut buf).await?;
        if n == 0 {
            anyhow::bail!("proxy closed before CONNECT response");
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 65536 {
            anyhow::bail!("CONNECT response too large");
        }
    }
    let status_line = std::str::from_utf8(&buf)
        .ok()
        .and_then(|s| s.lines().next())
        .unwrap_or("");
    if status_line.contains(" 200") {
        Ok(s)
    } else {
        anyhow::bail!("CONNECT rejected: {status_line}")
    }
}

// ---------------------------------------------------------------------------
// http
// ---------------------------------------------------------------------------

async fn run_http(proxy: &str, api_key: &str, url: &str, conns: u32, requests: u64, no_keepalive: bool) {
    let (host, port, path) = parse_url(url);
    let counter = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let lats: Arc<Mutex<Vec<f64>>> = Arc::default();

    let start = Instant::now();
    let mut set = JoinSet::new();
    for _ in 0..conns {
        let proxy = proxy.to_string();
        let api_key = api_key.to_string();
        let host = host.clone();
        let path = path.clone();
        let counter = counter.clone();
        let errors = errors.clone();
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
                    Ok(()) => lats.lock().unwrap().push(t0.elapsed().as_secs_f64()),
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
            errors.fetch_add(1, Ordering::Relaxed);
            eprintln!("task error: {e}");
        }
    }
    let elapsed = start.elapsed();
    let done = requests;
    println!(
        "http: requests={done} errors={} rps={:.0} elapsed={:.2}s",
        errors.load(Ordering::Relaxed),
        done as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64(),
    );
    print_latency("http latency", lats.lock().unwrap().clone());
}

async fn http_get(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    path: &str,
    api_key: &str,
    keepalive: bool,
) -> anyhow::Result<()> {
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
    let code = resp_code(&buf);
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
    Ok(())
}

fn resp_code(buf: &[u8]) -> u32 {
    let line = std::str::from_utf8(buf)
        .ok()
        .and_then(|s| s.lines().next())
        .unwrap_or("");
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// flood
// ---------------------------------------------------------------------------

async fn run_flood(proxy: &str, api_key: &str, target: &str, conns: u32, duration: u64, chunk: usize) {
    let deadline = Instant::now() + Duration::from_secs(duration);
    let total_bytes = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let est_lats: Arc<Mutex<Vec<f64>>> = Arc::default();

    let mut set = JoinSet::new();
    for _ in 0..conns {
        let proxy = proxy.to_string();
        let api_key = api_key.to_string();
        let target = target.to_string();
        let total_bytes = total_bytes.clone();
        let errors = errors.clone();
        let est_lats = est_lats.clone();
        set.spawn(async move {
            let t0 = Instant::now();
            let mut stream = match connect_via_proxy(&proxy, &target, &api_key).await {
                Ok(s) => s,
                Err(e) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("flood connect error: {e}");
                    return;
                }
            };
            est_lats.lock().unwrap().push(t0.elapsed().as_secs_f64());
            let buf = vec![0x5au8; chunk.max(1024)];
            let mut sent: u64 = 0;
            while Instant::now() < deadline {
                match stream.write(&buf).await {
                    Ok(0) => break,
                    Ok(n) => sent += n as u64,
                    Err(_) => break,
                }
            }
            total_bytes.fetch_add(sent, Ordering::Relaxed);
        });
    }
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            eprintln!("task error: {e}");
        }
    }
    let bytes = total_bytes.load(Ordering::Relaxed);
    let gbps = bytes as f64 * 8.0 / duration as f64 / 1e9;
    println!(
        "flood: conns={conns} duration={duration}s bytes={bytes} throughput={gbps:.3} Gbps errors={}",
        errors.load(Ordering::Relaxed),
    );
    print_latency("flood connect latency", est_lats.lock().unwrap().clone());
}

// ---------------------------------------------------------------------------
// hold
// ---------------------------------------------------------------------------

async fn run_hold(proxy: &str, api_key: &str, target: &str, conns: u32, duration: u64, ramp: u32) {
    let deadline = Instant::now() + Duration::from_secs(duration);
    let started = Instant::now();
    let opened = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let lats: Arc<Mutex<Vec<f64>>> = Arc::default();
    let sem = Arc::new(Semaphore::new(ramp as usize));

    let mut set = JoinSet::new();
    for _ in 0..conns {
        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };
        let proxy = proxy.to_string();
        let api_key = api_key.to_string();
        let target = target.to_string();
        let opened = opened.clone();
        let failed = failed.clone();
        let lats = lats.clone();
        set.spawn(async move {
            let _permit = permit;
            let t0 = Instant::now();
            match connect_via_proxy(&proxy, &target, &api_key).await {
                Ok(stream) => {
                    lats.lock().unwrap().push(t0.elapsed().as_secs_f64());
                    opened.fetch_add(1, Ordering::Relaxed);
                    let _ = tokio::time::sleep_until(deadline.into()).await;
                    drop(stream);
                }
                Err(_) => {
                    failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            eprintln!("task error: {e}");
        }
    }
    let opened = opened.load(Ordering::Relaxed);
    let failed = failed.load(Ordering::Relaxed);
    let open_elapsed = started.elapsed().as_secs_f64();
    println!(
        "hold: opened={opened} failed={failed} rate={:.0} conns/s elapsed={open_elapsed:.1}s",
        opened as f64 / open_elapsed.max(0.001),
    );
    print_latency("hold connect latency", lats.lock().unwrap().clone());
}
