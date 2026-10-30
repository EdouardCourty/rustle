//! Server tests: in-process end to end, one file per feature. Shared
//! helpers (config, origin, TLS and h2 clients) live here.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod accesslog;
mod http2;
mod policy;
mod reload;
mod sharding;
mod socks_udp;
mod tls;
mod uploads;

fn test_config(workers: usize, max_connections: usize) -> Arc<Config> {
    let mut cfg = Config::default();
    cfg.server.listen = "127.0.0.1:0".to_string();
    cfg.server.admin_listen = String::new();
    cfg.server.control_socket = String::new();
    cfg.server.workers = workers;
    cfg.limits.max_connections = max_connections;
    // Tests talk to origins on 127.0.0.1.
    cfg.filter.block_private = false;
    cfg.log.access = crate::accesslog::Level::Off;
    Arc::new(cfg)
}

/// Sends a request the proxy answers without an upstream (400) and
/// returns the status line.
async fn roundtrip(addr: SocketAddr, raw: &[u8]) -> String {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(raw).await.unwrap();
    let mut out = Vec::new();
    c.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

// ---- HTTPS proxy listener (end to end through rustls) ----

/// Self-signed cert for "localhost", written to a unique temp dir;
/// returns (cert path, key path, DER for the client's trust store).
fn tls_files() -> (String, String, rustls::pki_types::CertificateDer<'static>) {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "rustle-tls-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let (c, k) = (dir.join("cert.pem"), dir.join("key.pem"));
    std::fs::write(&c, ck.cert.pem()).unwrap();
    std::fs::write(&k, ck.signing_key.serialize_pem()).unwrap();
    let der = ck.cert.der().clone();
    (
        c.to_string_lossy().into_owned(),
        k.to_string_lossy().into_owned(),
        der,
    )
}

async fn tls_connect(
    addr: SocketAddr,
    der: rustls::pki_types::CertificateDer<'static>,
) -> tokio_rustls::client::TlsStream<TcpStream> {
    tls_connect_alpn(addr, der, &[]).await
}

async fn tls_connect_alpn(
    addr: SocketAddr,
    der: rustls::pki_types::CertificateDer<'static>,
    alpn: &[&[u8]],
) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut cfg = cfg;
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let conn = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let tcp = TcpStream::connect(addr).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    conn.connect(name, tcp).await.unwrap()
}

/// Minimal keep-alive origin: `/chunked` answers chunked, anything
/// else a 1000-byte Content-Length body.
async fn origin() -> SocketAddr {
    let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = ln.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = ln.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                loop {
                    let mut tmp = [0u8; 4096];
                    let n = match s.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                        buf.drain(..end + 4);
                        let resp: Vec<u8> = if head.contains("/chunked") {
                            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n".to_vec()
                        } else {
                            let mut r = b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n".to_vec();
                            r.extend(std::iter::repeat_n(b'x', 1000));
                            r
                        };
                        if s.write_all(&resp).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    addr
}

/// Reads one response (Content-Length or chunked, test-grade parser);
/// returns (status line, body with chunked framing kept).
async fn read_resp<S: tokio::io::AsyncRead + Unpin>(s: &mut S) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).into_owned();
            let mut body = buf[end + 4..].to_vec();
            let status = head.lines().next().unwrap_or("").to_string();
            let lower = head.to_ascii_lowercase();
            let done = |b: &Vec<u8>| {
                if lower.contains("transfer-encoding: chunked") {
                    b.ends_with(b"0\r\n\r\n")
                } else {
                    let cl = lower
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .map(|v| v.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    b.len() >= cl
                }
            };
            while !done(&body) {
                let n = s.read(&mut tmp).await.unwrap();
                assert!(n > 0, "EOF mid-body");
                body.extend_from_slice(&tmp[..n]);
            }
            return (status, body);
        }
        let n = s.read(&mut tmp).await.unwrap();
        assert!(n > 0, "EOF before head");
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn tls_config() -> (Arc<Config>, rustls::pki_types::CertificateDer<'static>) {
    let (cert, key, der) = tls_files();
    let mut cfg = (*test_config(2, 1000)).clone();
    cfg.tls.listen = "127.0.0.1:0".to_string();
    cfg.tls.cert = cert;
    cfg.tls.key = key;
    (Arc::new(cfg), der)
}

// ---- HTTP/2 (ALPN h2 on the HTTPS listener) ----

async fn h2_client(
    addr: SocketAddr,
    der: rustls::pki_types::CertificateDer<'static>,
) -> h2::client::SendRequest<bytes::Bytes> {
    let tls = tls_connect_alpn(addr, der, &[b"h2"]).await;
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let (send, conn) = h2::client::handshake(tls).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    send
}

async fn h2_body(mut body: h2::RecvStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        let _ = body.flow_control().release_capacity(chunk.len());
        out.extend_from_slice(&chunk);
    }
    out
}

// ---- access policy, bans, slowloris, SOCKS5 ----

async fn send_recv(addr: SocketAddr, raw: &[u8]) -> Vec<u8> {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(raw).await.unwrap();
    let mut out = Vec::new();
    let _ = c.read_to_end(&mut out).await;
    out
}

fn status(out: &[u8]) -> String {
    String::from_utf8_lossy(out)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

/// SOCKS5 CONNECT with username/password; returns (stream, REP code).
async fn socks_connect(
    addr: SocketAddr,
    pass: Option<&str>,
    target: SocketAddr,
) -> (TcpStream, u8) {
    let mut c = TcpStream::connect(addr).await.unwrap();
    let method = if pass.is_some() { 2 } else { 0 };
    c.write_all(&[5, 1, method]).await.unwrap();
    let mut sel = [0u8; 2];
    c.read_exact(&mut sel).await.unwrap();
    assert_eq!(sel, [5, method]);
    if let Some(p) = pass {
        let mut m = vec![1, 1, b'u', p.len() as u8];
        m.extend_from_slice(p.as_bytes());
        c.write_all(&m).await.unwrap();
        let mut st = [0u8; 2];
        c.read_exact(&mut st).await.unwrap();
        if st[1] != 0 {
            return (c, 0xff);
        }
    }
    let SocketAddr::V4(t) = target else { panic!() };
    let mut req = vec![5, 1, 0, 1];
    req.extend_from_slice(&t.ip().octets());
    req.extend_from_slice(&t.port().to_be_bytes());
    c.write_all(&req).await.unwrap();
    let mut rep = [0u8; 10];
    c.read_exact(&mut rep).await.unwrap();
    (c, rep[1])
}
