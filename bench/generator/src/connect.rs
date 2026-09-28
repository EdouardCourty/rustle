//! Dial helpers shared by every load mode: parse targets, resolve the
//! upstream once (benchmark purity: measure the proxy, not DNS), and open
//! CONNECT tunnels through the proxy under test.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub(crate) fn parse_url(url: &str) -> (String, u16, String) {
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
        Some(i) => (
            authority[..i].to_string(),
            authority[i + 1..].parse().unwrap(),
        ),
        None => (authority.to_string(), 80),
    };
    (host, port, path)
}

/// Resolve a host:port target ONCE to a numeric ip:port string, so the
/// benchmark measures the proxy itself and not per-connection DNS
/// (myproxy would do getaddrinfo per request; 3proxy caches via nscache).
pub(crate) async fn resolve_once(target: &str) -> anyhow::Result<String> {
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
pub(crate) async fn connect_via_proxy(
    proxy: &str,
    target: &str,
    api_key: &str,
) -> anyhow::Result<TcpStream> {
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
