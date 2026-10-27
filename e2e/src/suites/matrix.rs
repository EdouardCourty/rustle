//! Matrix: proxy configs × client transports × operations, each operation
//! looping over payload sizes chosen at the engine boundaries (8 KB first
//! buffer, 64 KB copy cap, 128 KB splice switch, bulk).
//!
//! Name: `matrix/<config>/<transport>/<op>`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use super::{quick_mode, H1};
use crate::clients::http1::{self, Client};
use crate::clients::socks5::{self, Outcome, Target};
use crate::clients::{self as cl, h2 as h2c};
use crate::harness::{certs, payload, Config, Env, Proxy};
use crate::runner::{scenario, skip, Scenario};

const FULL: &[usize] = &[
    0,
    1,
    8191,
    8193,
    65535,
    65537,
    131071,
    131073,
    1 << 20,
    10 << 20,
];
const QUICK: &[usize] = &[0, 8193, 131073, 1 << 20];
const BIG: usize = 64 << 20;

#[derive(Clone, Copy)]
struct Variant {
    name: &'static str,
    apply: fn(Config, &Env) -> Config,
    /// Clients authenticate (false: open relay).
    auth: bool,
    /// Targets by name (`localhost:port`) instead of IP.
    names: bool,
    transports: &'static [T],
    /// Adds the 64 MB size.
    big: bool,
    quick: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum T {
    H1(H1),
    H2,
    Socks,
}

impl T {
    fn name(self) -> &'static str {
        match self {
            T::H1(h) => h.name(),
            T::H2 => "h2",
            T::Socks => "socks5",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Get(&'static str),
    UploadCl,
    UploadChunked,
    UploadNoLen,
    Connect,
    ConnectTls,
    Udp,
}

impl Op {
    fn name(self) -> String {
        match self {
            Op::Get(f) => format!("get-{f}"),
            Op::UploadCl => "upload-cl".into(),
            Op::UploadChunked => "upload-chunked".into(),
            Op::UploadNoLen => "upload-nolen".into(),
            Op::Connect => "connect".into(),
            Op::ConnectTls => "connect-tls".into(),
            Op::Udp => "udp".into(),
        }
    }
}

const ALL: &[T] = &[T::H1(H1::Plain), T::H1(H1::Tls), T::H2, T::Socks];
const HTTP: &[T] = &[T::H1(H1::Plain), T::H1(H1::Tls), T::H2];
const TLS_ONLY: &[T] = &[T::H1(H1::Tls), T::H1(H1::Tls12), T::H2];

fn id(c: Config, _: &Env) -> Config {
    c
}

fn variants() -> Vec<Variant> {
    let v = |name, apply| Variant {
        name,
        apply,
        auth: true,
        names: false,
        transports: ALL,
        big: false,
        quick: false,
    };
    vec![
        Variant {
            transports: &[
                T::H1(H1::Plain),
                T::H1(H1::Tls),
                T::H1(H1::Tls12),
                T::H2,
                T::Socks,
            ],
            big: true,
            quick: true,
            // The 64 MiB tunnels run on a shared runner next to hundreds of other scenarios:
            // a stall of the test process longer than the 10 s idle timeout of the harness
            // config must not be mistaken for a proxy failure.
            ..v("default", |c, _| c.set("timeouts", "idle", "\"60s\""))
        },
        v("workers2-pin", |c, _| {
            c.set("server", "workers", "2")
                .set("server", "pin_cores", "true")
        }),
        v("workers0", |c, _| c.set("server", "workers", "0")),
        Variant {
            transports: HTTP,
            ..v("keepalive-off", |c, _| {
                c.set("http", "upstream_keepalive", "false")
            })
        },
        v("sockbuf-autotune", |c, _| {
            c.set("server", "socket_buffer_bytes", "0")
        }),
        Variant {
            transports: TLS_ONLY,
            ..v("ktls", |c, _| c.set("tls", "ktls", "true"))
        },
        Variant {
            auth: false,
            ..v("open-relay", |c, _| c.set("auth", "api_keys", "[]"))
        },
        Variant {
            names: true,
            ..v("dns-names", id)
        },
        Variant {
            names: true,
            ..v("dns-nocache", |c, _| c.set("dns", "ttl", "\"0s\""))
        },
        v("log-all-file", |c, _| {
            let f = format!("{:?}", c.path("access.log").to_string_lossy());
            c.set("log", "access", "\"all\"").set("log", "file", &f)
        }),
        v("log-all-stdout", |c, _| {
            c.set("log", "access", "\"all\"")
                .set("log", "output", "\"stdout\"")
                .set("log", "target", "\"url\"")
        }),
        v("policy", |c, e| {
            let ports = [
                e.origin.v4.port(),
                e.targets.echo.port(),
                e.targets.tls_echo.port(),
                e.targets.udp.port(),
            ];
            c.set("filter", "allowed_ports", &format!("{ports:?}"))
                .set("filter", "deny", "[\"192.0.2.0/24\", \"2001:db8::/32\"]")
                .set("auth", "allow_clients", "[\"127.0.0.0/8\", \"::1/128\"]")
        }),
        v("limits-generous", |c, _| {
            c.set("http", "max_request_body_bytes", "104857600")
                .set("http", "max_response_body_bytes", "104857600")
                .set("limits", "max_header_bytes", "16384")
        }),
    ]
}

fn ops(t: T) -> &'static [Op] {
    match t {
        T::H1(_) => &[
            Op::Get("cl"),
            Op::Get("chunked"),
            Op::Get("chunked-small"),
            Op::Get("eof"),
            Op::UploadCl,
            Op::UploadChunked,
            Op::Connect,
            Op::ConnectTls,
        ],
        T::H2 => &[
            Op::Get("cl"),
            Op::Get("chunked"),
            Op::Get("chunked-small"),
            Op::Get("eof"),
            Op::UploadCl,
            Op::UploadNoLen,
            Op::Connect,
            Op::ConnectTls,
        ],
        T::Socks => &[Op::Connect, Op::ConnectTls, Op::Udp],
    }
}

pub fn scenarios() -> Vec<Scenario> {
    let mut out = Vec::new();
    for v in variants() {
        for &t in v.transports {
            for &op in ops(t) {
                let name = format!("matrix/{}/{}/{}", v.name, t.name(), op.name());
                let s = scenario(name, move |env| run(env, v, t, op))
                    .timeout(Duration::from_secs(if v.big { 180 } else { 90 }));
                out.push(if v.quick && t != T::H1(H1::Tls12) {
                    s.quick()
                } else {
                    s
                });
            }
        }
    }
    out
}

fn sizes(v: &Variant, op: Op) -> Vec<usize> {
    let mut s = if quick_mode() {
        QUICK.to_vec()
    } else {
        FULL.to_vec()
    };
    match op {
        // Tiny bodies one byte at a time are slow and test nothing new.
        Op::Get("chunked-small") => s.retain(|&n| n <= 1 << 20),
        // macOS caps a UDP send at net.inet.udp.maxdgram (9216).
        Op::Udp if cfg!(target_os = "linux") => s = vec![1, 512, 1472, 8192, 65000],
        Op::Udp => s = vec![1, 512, 1472, 8192],
        _ => {
            if v.big && !quick_mode() {
                s.push(BIG);
            }
        }
    }
    s
}

struct Ctx {
    env: Arc<Env>,
    v: Variant,
    auth: Option<String>,
}

impl Ctx {
    /// `host:port` of an IPv4 target, by name in `names` variants.
    fn authority(&self, a: std::net::SocketAddr) -> String {
        if self.v.names {
            format!("localhost:{}", a.port())
        } else {
            a.to_string()
        }
    }

    fn socks_target(&self, a: std::net::SocketAddr) -> Target {
        if self.v.names {
            Target::Name("localhost".into(), a.port())
        } else {
            Target::Ip(a)
        }
    }

    fn socks_auth(&self) -> Option<(&str, &str)> {
        self.v.auth.then_some(("u", crate::harness::proxy::KEY))
    }
}

async fn run(env: Arc<Env>, v: Variant, t: T, op: Op) -> anyhow::Result<()> {
    if v.name == "ktls" && !env.ktls {
        return Err(skip("kTLS unavailable (kernel module tls)"));
    }
    let cfg = (v.apply)(Config::new(), &env);
    let p = Proxy::start(cfg).await?;
    let cx = Ctx {
        auth: v.auth.then(http1::default_auth),
        env,
        v,
    };
    for n in sizes(&v, op) {
        let what = format!("{} bytes", n);
        match (t, op) {
            (T::H1(h), Op::Get(f)) => get_h1(&cx, &p, h, f, n).await,
            (T::H2, Op::Get(f)) => get_h2(&cx, &p, f, n).await,
            (T::H1(h), Op::UploadCl | Op::UploadChunked) => {
                upload_h1(&cx, &p, h, matches!(op, Op::UploadChunked), n).await
            }
            (T::H2, Op::UploadCl | Op::UploadNoLen) => {
                upload_h2(&cx, &p, matches!(op, Op::UploadCl), n).await
            }
            (_, Op::Connect | Op::ConnectTls) => {
                tunnel(&cx, &p, t, matches!(op, Op::ConnectTls), n).await
            }
            (T::Socks, Op::Udp) => udp(&cx, &p, n).await,
            _ => unreachable!("{t:?} {op:?}"),
        }
        .with_context(|| what)?;
    }
    if v.name == "log-all-file" {
        let log = std::fs::read_to_string(p.dir.join("access.log")).unwrap_or_default();
        p.wait_for("access log lines", Duration::from_secs(3), |p| {
            std::fs::read_to_string(p.dir.join("access.log")).is_ok_and(|l| !l.is_empty())
        })
        .await
        .with_context(|| format!("access log: {log:?}"))?;
    }
    if v.name == "log-all-stdout" {
        p.wait_for("access log on stdout", Duration::from_secs(3), |p| {
            p.stdout().contains("client=")
        })
        .await?;
    }
    p.finish().await
}

fn data_url(cx: &Ctx, framing: &str, n: usize) -> String {
    let (f, chunk) = match framing {
        "chunked-small" => ("chunked", 1000),
        f => (f, 16 * 1024),
    };
    format!(
        "http://{}/data/{n}?framing={f}&chunk={chunk}&seed={}",
        cx.authority(cx.env.origin.v4),
        n + 7
    )
}

async fn get_h1(cx: &Ctx, p: &Proxy, h: H1, framing: &str, n: usize) -> anyhow::Result<()> {
    let mut c = h.client(p).await?;
    // Two requests on the same connection (keep-alive), unless EOF-framed.
    let rounds = if framing == "eof" { 1 } else { 2 };
    for _ in 0..rounds {
        let req = http1::request(
            "GET",
            &data_url(cx, framing, n),
            cx.auth.as_deref(),
            "",
            b"",
        );
        let t0 = std::time::Instant::now();
        let r = c.roundtrip(&req, false).await?;
        // An EOF-delimited body ends when the proxy closes: promptly, not
        // at the idle timeout.
        // 5 s plus 1 s per MB: the point is to catch a proxy sitting on the
        // idle timeout, not to time a 10 MB transfer on a runner that is
        // running hundreds of scenarios in parallel.
        let limit = Duration::from_secs(5) + Duration::from_secs((n / 1_000_000) as u64);
        anyhow::ensure!(
            t0.elapsed() < limit,
            "response took {:?} (limit {limit:?})",
            t0.elapsed()
        );
        r.expect(200)?;
        payload::check(n as u64 + 7, n, &r.body)?;
    }
    if framing == "eof" {
        anyhow::ensure!(
            c.closed(Duration::from_secs(5)).await,
            "connection kept open after an EOF-framed response"
        );
    }
    Ok(())
}

fn h2_auth(cx: &Ctx) -> Vec<(&str, &str)> {
    match &cx.auth {
        Some(a) => vec![("proxy-authorization", a.as_str())],
        None => vec![],
    }
}

async fn get_h2(cx: &Ctx, p: &Proxy, framing: &str, n: usize) -> anyhow::Result<()> {
    let mut s = h2c::connect(p.tls()).await?;
    for _ in 0..2 {
        let r = h2c::request(&mut s, "GET", &data_url(cx, framing, n), &h2_auth(cx), None).await?;
        anyhow::ensure!(r.status == 200, "status {}", r.status);
        payload::check(n as u64 + 7, n, &r.body)?;
    }
    Ok(())
}

/// Splits an `/echo` body: (request head seen by the origin, body).
pub fn split_echo(body: &[u8]) -> anyhow::Result<(String, &[u8])> {
    let end = body
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("echo body without head")?;
    Ok((
        String::from_utf8_lossy(&body[..end]).into_owned(),
        &body[end + 4..],
    ))
}

fn check_forwarded_head(head: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !head.to_ascii_lowercase().contains("proxy-authorization"),
        "Proxy-Authorization leaked to the origin:\n{head}"
    );
    Ok(())
}

async fn upload_h1(cx: &Ctx, p: &Proxy, h: H1, chunked: bool, n: usize) -> anyhow::Result<()> {
    let mut c = h.client(p).await?;
    let body = payload::make(n as u64 + 11, n);
    let url = format!("http://{}/echo", cx.authority(cx.env.origin.v4));
    let req = if chunked {
        let mut r = format!(
            "POST {url} HTTP/1.1\r\nHost: x\r\n{}Transfer-Encoding: chunked\r\n\r\n",
            http1::auth_header(cx.auth.as_deref())
        )
        .into_bytes();
        r.extend(http1::chunked(&body, 16 * 1024 + 3, None));
        r
    } else {
        http1::request("POST", &url, cx.auth.as_deref(), "", &body)
    };
    // Twice on the same connection: the upload leaves it reusable.
    for _ in 0..2 {
        let r = c.roundtrip(&req, false).await?;
        r.expect(200)?;
        let want = if chunked { "chunked" } else { "cl" };
        anyhow::ensure!(
            r.header("x-req-framing") == Some(want),
            "origin saw framing {:?}, want {want}",
            r.header("x-req-framing")
        );
        let (head, got) = split_echo(&r.body)?;
        check_forwarded_head(&head)?;
        payload::check(n as u64 + 11, n, got).context("uploaded body at the origin")?;
    }
    Ok(())
}

async fn upload_h2(cx: &Ctx, p: &Proxy, with_len: bool, n: usize) -> anyhow::Result<()> {
    let mut s = h2c::connect(p.tls()).await?;
    let body = payload::make(n as u64 + 11, n);
    let url = format!("http://{}/echo", cx.authority(cx.env.origin.v4));
    let r = h2c::request(&mut s, "POST", &url, &h2_auth(cx), Some((&body, with_len))).await?;
    anyhow::ensure!(r.status == 200, "status {}", r.status);
    let framing = r
        .headers
        .get("x-req-framing")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ok = if with_len {
        framing == "cl"
    } else {
        framing == "chunked" || n == 0
    };
    anyhow::ensure!(ok, "origin saw framing {framing:?} (with_len={with_len})");
    let (head, got) = split_echo(&r.body)?;
    check_forwarded_head(&head)?;
    payload::check(n as u64 + 11, n, got).context("uploaded body at the origin")
}

/// Opens a tunnel to `target` over transport `t` (asserting success).
pub async fn open_tunnel(
    p: &Proxy,
    t_name: &str,
    authority: &str,
    socks_target: Target,
    auth: Option<&str>,
    socks_auth: Option<(&str, &str)>,
) -> anyhow::Result<cl::BoxIo> {
    Ok(match t_name {
        "h1" | "tls" | "tls12" => {
            let h = match t_name {
                "h1" => H1::Plain,
                "tls" => H1::Tls,
                _ => H1::Tls12,
            };
            let mut c: Client = h.client(p).await?;
            let st = c.connect(authority, auth).await?;
            anyhow::ensure!(st == 200, "CONNECT status {st}");
            c.into_stream()?
        }
        "h2" => {
            let mut s = h2c::connect(p.tls()).await?;
            let (st, t) = h2c::connect_tunnel(&mut s, authority, auth).await?;
            anyhow::ensure!(st == 200, "h2 CONNECT status {st}");
            Box::new(t.unwrap())
        }
        _ => {
            let (s, o) = socks5::connect(p.socks(), socks_auth, &socks_target).await?;
            anyhow::ensure!(o == Outcome::Rep(0), "SOCKS5 CONNECT: {o:?}");
            Box::new(s)
        }
    })
}

async fn tunnel(cx: &Ctx, p: &Proxy, t: T, tls: bool, n: usize) -> anyhow::Result<()> {
    let target = if tls {
        cx.env.targets.tls_echo
    } else {
        cx.env.targets.echo
    };
    let s = open_tunnel(
        p,
        t.name(),
        &cx.authority(target),
        cx.socks_target(target),
        cx.auth.as_deref(),
        cx.socks_auth(),
    )
    .await?;
    if tls {
        let t = cl::tls_over(s, certs::client(&[])).await?;
        cl::echo_roundtrip(t, n as u64 + 3, n).await
    } else {
        cl::echo_roundtrip(s, n as u64 + 3, n).await
    }
}

async fn udp(cx: &Ctx, p: &Proxy, n: usize) -> anyhow::Result<()> {
    let (_ctl, relay) = socks5::udp_associate(p.socks(), cx.socks_auth()).await?;
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let data = payload::make(n as u64, n);
    let pkt = socks5::udp_packet(&cx.socks_target(cx.env.targets.udp), &data);
    let mut buf = vec![0u8; 70000];
    for _attempt in 0..3 {
        sock.send_to(&pkt, relay).await?;
        if let Ok(r) = tokio::time::timeout(Duration::from_secs(1), sock.recv_from(&mut buf)).await
        {
            let (k, _) = r?;
            let (from, got) = socks5::udp_parse(&buf[..k])?;
            anyhow::ensure!(
                from.port() == cx.env.targets.udp.port(),
                "reply source {from}"
            );
            return payload::check(n as u64, n, got);
        }
    }
    anyhow::bail!("no UDP echo after 3 attempts")
}
