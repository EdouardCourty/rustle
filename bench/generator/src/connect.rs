//! Dial helpers shared by every load mode: parse targets, resolve the
//! upstream once (benchmark purity: measure the proxy, not DNS), open the
//! connection to the proxy (plain TCP, or TLS with `--proxy-tls`) and
//! CONNECT tunnels through it.

use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

/// Set once from the CLI: every proxy connection goes through TLS.
static PROXY_TLS: OnceLock<tokio_rustls::TlsConnector> = OnceLock::new();
/// Set once from the CLI: HTTP/2 (ALPN h2) over that TLS.
static PROXY_H2: OnceLock<()> = OnceLock::new();

/// Enables TLS towards the proxy (HTTPS proxy benchmark), optionally
/// HTTP/2. The proxy's certificate is not validated (self-signed bench
/// certificates); the handshake signatures are, so the crypto cost stays
/// the real one.
pub(crate) fn enable_proxy_tls(h2: bool) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("tls versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
        .with_no_client_auth();
    if h2 {
        cfg.alpn_protocols = vec![b"h2".to_vec()];
        let _ = PROXY_H2.set(());
    }
    let _ = PROXY_TLS.set(tokio_rustls::TlsConnector::from(Arc::new(cfg)));
}

pub(crate) fn h2_enabled() -> bool {
    PROXY_H2.get().is_some()
}

fn io_err<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// Opens an HTTP/2 connection to the proxy (the connection driver runs in
/// its own task).
pub(crate) async fn h2_connect(
    proxy: &str,
) -> std::io::Result<h2::client::SendRequest<bytes::Bytes>> {
    let s = connect_proxy(proxy).await?;
    let (send, conn) = h2::client::handshake(s).await.map_err(io_err)?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    send.ready().await.map_err(io_err)
}

/// A CONNECT tunnel carried by one HTTP/2 stream.
pub(crate) struct H2Tunnel {
    _conn: h2::client::SendRequest<bytes::Bytes>,
    send: h2::SendStream<bytes::Bytes>,
    recv: h2::RecvStream,
    rbuf: bytes::Bytes,
}

#[derive(Debug)]
struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// A connection to the proxy under test (or, for H2, one tunnel stream).
pub(crate) enum ProxyStream {
    Tcp(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    H2(Box<H2Tunnel>),
}

/// TCP connect to the proxy (+ TLS handshake with `--proxy-tls`).
pub(crate) async fn connect_proxy(proxy: &str) -> std::io::Result<ProxyStream> {
    let s = TcpStream::connect(proxy).await?;
    s.set_nodelay(true)?;
    match PROXY_TLS.get() {
        None => Ok(ProxyStream::Tcp(s)),
        Some(tls) => {
            let host = proxy.rsplit_once(':').map(|(h, _)| h).unwrap_or(proxy);
            let name = rustls::pki_types::ServerName::try_from(host.to_string())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
            Ok(ProxyStream::Tls(Box::new(tls.connect(name, s).await?)))
        }
    }
}

impl AsyncRead for ProxyStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            ProxyStream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            ProxyStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
            ProxyStream::H2(t) => {
                if t.rbuf.is_empty() {
                    match std::task::ready!(t.recv.poll_data(cx)) {
                        Some(Ok(b)) => {
                            let _ = t.recv.flow_control().release_capacity(b.len());
                            t.rbuf = b;
                        }
                        Some(Err(e)) => return Poll::Ready(Err(io_err(e))),
                        None => return Poll::Ready(Ok(())),
                    }
                }
                let k = t.rbuf.len().min(buf.remaining());
                buf.put_slice(&t.rbuf.split_to(k));
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl AsyncWrite for ProxyStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            ProxyStream::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            ProxyStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
            ProxyStream::H2(t) => {
                t.send.reserve_capacity(buf.len());
                match std::task::ready!(t.send.poll_capacity(cx)) {
                    Some(Ok(0)) => {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                    Some(Ok(n)) => {
                        let n = n.min(buf.len());
                        t.send
                            .send_data(bytes::Bytes::copy_from_slice(&buf[..n]), false)
                            .map_err(io_err)?;
                        Poll::Ready(Ok(n))
                    }
                    Some(Err(e)) => Poll::Ready(Err(io_err(e))),
                    None => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            ProxyStream::Tcp(s) => Pin::new(s).poll_flush(cx),
            ProxyStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
            ProxyStream::H2(_) => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            ProxyStream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            ProxyStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
            ProxyStream::H2(t) => {
                let _ = t.send.send_data(bytes::Bytes::new(), true);
                Poll::Ready(Ok(()))
            }
        }
    }
}

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
) -> anyhow::Result<ProxyStream> {
    if h2_enabled() {
        return tokio::time::timeout(
            std::time::Duration::from_secs(10),
            h2_tunnel(proxy, target, api_key),
        )
        .await
        .map_err(|_| anyhow::anyhow!("connect timeout"))?;
    }
    // 10 s connect timeout: without it, a proxy that drops SYNs (3proxy
    // over its ~17k wall) makes the kernel retransmit for ~127 s per
    // connection and the hold matrix crawls for minutes.
    let mut s = tokio::time::timeout(std::time::Duration::from_secs(10), connect_proxy(proxy))
        .await
        .map_err(|_| anyhow::anyhow!("connect timeout"))??;
    let auth = B64.encode(format!("bench:{api_key}"));
    let req = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {auth}\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await?;
    s.flush().await?;
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

/// CONNECT over HTTP/2: one h2 connection per tunnel (same connection
/// count as HTTP/1, so the tests compare transports, not multiplexing).
async fn h2_tunnel(proxy: &str, target: &str, api_key: &str) -> anyhow::Result<ProxyStream> {
    let send_req = h2_connect(proxy).await?;
    let auth = B64.encode(format!("bench:{api_key}"));
    let req = http::Request::builder()
        .method(http::Method::CONNECT)
        .uri(target)
        .header("proxy-authorization", format!("Basic {auth}"))
        .body(())?;
    let (resp, send) = send_req.clone().send_request(req, false)?;
    let resp = resp.await?;
    if resp.status() != 200 {
        anyhow::bail!("CONNECT rejected: {}", resp.status());
    }
    Ok(ProxyStream::H2(Box::new(H2Tunnel {
        _conn: send_req,
        send,
        recv: resp.into_body(),
        rbuf: bytes::Bytes::new(),
    })))
}
