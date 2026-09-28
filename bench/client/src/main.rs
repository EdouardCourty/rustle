use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
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
    /// Number of measured runs; the median RESULT is reported.
    #[arg(long, default_value_t = 3)]
    repeat: u32,
    /// Disable the warmup run.
    #[arg(long)]
    no_warmup: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Clone)]
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
    /// Open N CONNECT tunnels and hold them for a duration.
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
    /// TCP discard server (sink): accepts connections and drops all bytes.
    /// No process fork, no per-connection allocation.
    Sink {
        /// Listen address.
        #[arg(default_value = "0.0.0.0:9000")]
        listen: String,
    },
}

/// One measured run. Static fields describe the workload; numeric fields are
/// the measurements. Field layout is identical across runs of one mode, so
/// medians can be taken index-wise.
struct Metrics {
    statics: Vec<(&'static str, String)>,
    nums: Vec<(&'static str, f64)>,
}

impl Metrics {
    fn line(&self, tag: &str) -> String {
        let mut s = format!("RESULT {tag}");
        for (k, v) in &self.statics {
            s.push_str(&format!(" {k}={v}"));
        }
        for (k, v) in &self.nums {
            s.push_str(&format!(" {k}={v:.3}"));
        }
        s
    }
}

fn median_of(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    // NaN-tolerant: a run with zero successful connects yields NaN
    // percentiles; unwrap() on NaN.partial_cmp(NaN) would panic.
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[(v.len() - 1) / 2]
}

fn median_metrics(runs: Vec<Metrics>) -> Metrics {
    let statics = runs[0].statics.clone();
    let mut nums = Vec::with_capacity(runs[0].nums.len());
    for (i, (key, _)) in runs[0].nums.iter().enumerate() {
        let vals: Vec<f64> = runs.iter().map(|r| r.nums[i].1).collect();
        nums.push((*key, median_of(vals)));
    }
    Metrics { statics, nums }
}

/// (p50, p95, p99) in milliseconds from a latency vector in seconds.
fn pcts_ms(v: &[f64]) -> (f64, f64, f64) {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p = |q: f64| -> f64 {
        if s.is_empty() {
            f64::NAN
        } else {
            let idx = ((s.len() - 1) as f64 * q).round() as usize;
            s[idx] * 1e3
        }
    };
    (p(0.50), p(0.95), p(0.99))
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Cmd::Sink { listen } = &cli.cmd {
        run_sink(listen).await;
    } else {
        if !cli.no_warmup {
            if let Some(w) = warmup_of(&cli.cmd) {
                eprintln!("--- warmup");
                let _ = run_once(&cli, &w).await;
            }
        }
        let mut runs = Vec::new();
        for k in 1..=cli.repeat {
            let m = run_once(&cli, &cli.cmd).await;
            println!("{}", m.line(&format!("run={k}")));
            runs.push(m);
        }
        println!("{}", median_metrics(runs).line("median"));
    }
}

fn warmup_of(cmd: &Cmd) -> Option<Cmd> {
    match cmd {
        Cmd::Http {
            url,
            conns,
            requests,
            no_keepalive,
        } => Some(Cmd::Http {
            url: url.clone(),
            conns: *conns,
            requests: (*requests).min(1000),
            no_keepalive: *no_keepalive,
        }),
        Cmd::Flood {
            target,
            conns,
            duration: _,
            chunk,
        } => Some(Cmd::Flood {
            target: target.clone(),
            conns: *conns,
            duration: 2,
            chunk: *chunk,
        }),
        Cmd::Hold { .. } => None, // opening connections IS the test
        Cmd::Sink { .. } => None,
    }
}

async fn run_once(cli: &Cli, cmd: &Cmd) -> Metrics {
    match cmd {
        Cmd::Http {
            url,
            conns,
            requests,
            no_keepalive,
        } => run_http(&cli.proxy, &cli.api_key, url, *conns, *requests, *no_keepalive).await,
        Cmd::Flood {
            target,
            conns,
            duration,
            chunk,
        } => run_flood(&cli.proxy, &cli.api_key, target, *conns, *duration, *chunk).await,
        Cmd::Hold {
            target,
            conns,
            duration,
            ramp,
        } => run_hold(&cli.proxy, &cli.api_key, target, *conns, *duration, *ramp).await,
        Cmd::Sink { .. } => unreachable!(),
    }
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

/// Resolve a host:port target ONCE to a numeric ip:port string, so the
/// benchmark measures the proxy itself and not per-connection DNS
/// (myproxy would do getaddrinfo per request; 3proxy caches via nscache).
async fn resolve_once(target: &str) -> anyhow::Result<String> {
    let (host, port) = match target.rfind(':') {
        Some(i) => (&target[..i], target[i + 1..].parse::<u16>()?),
        None => anyhow::bail!("target must be host:port: {target}"),
    };
    // Numeric target (IPv6 may be bracketed, e.g. "[::1]:9000"): keep as-is.
    let unbracketed = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'));
    let numeric = host.parse::<std::net::IpAddr>().is_ok()
        || unbracketed
            .map(|h| h.parse::<std::net::IpAddr>().is_ok())
            .unwrap_or(false);
    if numeric {
        return Ok(target.to_string());
    }
    let addr = tokio::net::lookup_host((host, port))
        .await?
        .next()
        .ok_or_else(|| anyhow::anyhow!("no address for {target}"))?;
    Ok(addr.to_string())
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

#[allow(clippy::too_many_arguments)]
async fn run_http(
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

// ---------------------------------------------------------------------------
// flood
// ---------------------------------------------------------------------------

async fn run_flood(
    proxy: &str,
    api_key: &str,
    target: &str,
    conns: u32,
    duration: u64,
    chunk: usize,
) -> Metrics {
    let deadline = Instant::now() + Duration::from_secs(duration);
    let target_owned = resolve_once(target)
        .await
        .unwrap_or_else(|e| panic!("resolve {target}: {e}"));
    let target: &str = &target_owned;
    let total_bytes = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let est_lats: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));

    let mut set = JoinSet::new();
    for _ in 0..conns {
        let proxy = proxy.to_string();
        let api_key = api_key.to_string();
        let target = target.to_string();
        let deadline = deadline;
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
    let lat = est_lats.lock().unwrap().clone();
    let (p50, p95, p99) = pcts_ms(&lat);
    Metrics {
        statics: vec![
            ("mode", "flood".to_string()),
            ("target", target.to_string()),
            ("conns", conns.to_string()),
            ("duration", duration.to_string()),
            ("chunk", chunk.to_string()),
        ],
        nums: vec![
            ("gbps", bytes as f64 * 8.0 / duration as f64 / 1e9),
            ("mb", bytes as f64 / 1e6),
            ("p50_ms", p50),
            ("p95_ms", p95),
            ("p99_ms", p99),
            ("errors", errors.load(Ordering::Relaxed) as f64),
        ],
    }
}

// ---------------------------------------------------------------------------
// hold
// ---------------------------------------------------------------------------

async fn run_hold(
    proxy: &str,
    api_key: &str,
    target: &str,
    conns: u32,
    duration: u64,
    ramp: u32,
) -> Metrics {
    let deadline = Instant::now() + Duration::from_secs(duration);
    let started = Instant::now();
    let target_owned = resolve_once(target)
        .await
        .unwrap_or_else(|e| panic!("resolve {target}: {e}"));
    let target: &str = &target_owned;
    let opened = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let lats: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    // Instant (as unix-ish ms) of the most recent successful open, so the
    // open rate uses the open window, not the total hold duration.
    let last_open_ms = Arc::new(AtomicU64::new(0));
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
        let deadline = deadline;
        let opened = opened.clone();
        let failed = failed.clone();
        let lats = lats.clone();
        let last_open_ms = last_open_ms.clone();
        set.spawn(async move {
            // The permit gates the CONNECT phase only, not the hold:
            // release it as soon as the tunnel is established.
            let _permit = permit;
            let t0 = Instant::now();
            let res = connect_via_proxy(&proxy, &target, &api_key).await;
            drop(_permit);
            match res {
                Ok(stream) => {
                    lats.lock().unwrap().push(t0.elapsed().as_secs_f64());
                    opened.fetch_add(1, Ordering::Relaxed);
                    last_open_ms.store(
                        started.elapsed().as_millis() as u64,
                        Ordering::Relaxed,
                    );
                    tokio::time::sleep_until(deadline.into()).await;
                    drop(stream);
                }
                Err(e) => {
                    eprintln!("hold connect error: {e}");
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
    let opened_n = opened.load(Ordering::Relaxed);
    let failed_n = failed.load(Ordering::Relaxed);
    let open_window_s =
        last_open_ms.load(Ordering::Relaxed) as f64 / 1000.0;
    let total_s = started.elapsed().as_secs_f64();
    let lat = lats.lock().unwrap().clone();
    let (p50, p95, p99) = pcts_ms(&lat);
    Metrics {
        statics: vec![
            ("mode", "hold".to_string()),
            ("target", target.to_string()),
            ("conns", conns.to_string()),
            ("duration", duration.to_string()),
            ("ramp", ramp.to_string()),
        ],
        nums: vec![
            ("opened", opened_n as f64),
            ("failed", failed_n as f64),
            ("rate", opened_n as f64 / open_window_s.max(0.001)),
            ("open_s", open_window_s),
            ("elapsed_s", total_s),
            ("p50_ms", p50),
            ("p95_ms", p95),
            ("p99_ms", p99),
        ],
    }
}

// ---------------------------------------------------------------------------
// sink
// ---------------------------------------------------------------------------

/// Bind with a large accept backlog (tokio/mio default is only 1024, which
/// drops SYNs under the hold test's connection bursts).
async fn bind_sink(listen: &str) -> anyhow::Result<TcpListener> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    let addr: std::net::SocketAddr = listen.parse()?;
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    let sa: SockAddr = addr.into();
    socket.bind(&sa)?;
    socket.listen(65535)?;
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    Ok(TcpListener::from_std(std_listener)?)
}

async fn run_sink(listen: &str) -> ! {
    let listener = match bind_sink(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("sink: bind {listen} failed: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("sink: listening on {listen}");
    loop {
        let (mut s, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("sink: accept error: {e}");
                continue;
            }
        };
        tokio::spawn(async move {
            let _ = s.set_nodelay(true);
            let mut buf = [0u8; 65536];
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
    }
}
