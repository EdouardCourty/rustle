//! Test clients. Each speaks one client protocol of the proxy and verifies
//! TLS certificates (the proxy's and the origins' are the same localhost
//! certificate).

pub mod curl;
pub mod h2;
pub mod http1;
pub mod socks5;

use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::harness::payload;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type BoxIo = Box<dyn Io>;

/// `Proxy-Authorization` value for `user:key`.
pub fn basic(user: &str, key: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{key}"))
    )
}

pub async fn tls_connect(
    addr: std::net::SocketAddr,
    cfg: std::sync::Arc<rustls::ClientConfig>,
) -> anyhow::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    tcp.set_nodelay(true)?;
    tls_over(tcp, cfg).await
}

pub async fn tls_over<S: AsyncRead + AsyncWrite + Unpin>(
    s: S,
    cfg: std::sync::Arc<rustls::ClientConfig>,
) -> anyhow::Result<tokio_rustls::client::TlsStream<S>> {
    let name = rustls::pki_types::ServerName::try_from("localhost")?;
    tokio_rustls::TlsConnector::from(cfg)
        .connect(name, s)
        .await
        .context("TLS handshake")
}

/// Pushes payload `seed` of `len` bytes through an echoing tunnel while
/// reading the echo concurrently, then half-closes and expects the echo
/// side's EOF: integrity in both directions and half-close propagation.
pub async fn echo_roundtrip<S: AsyncRead + AsyncWrite + Send>(
    s: S,
    seed: u64,
    len: usize,
) -> anyhow::Result<()> {
    let (mut r, mut w) = tokio::io::split(s);
    let writer = async move {
        let mut buf = vec![0u8; 64 * 1024];
        let mut off = 0;
        while off < len {
            let k = buf.len().min(len - off);
            payload::fill(seed, off as u64, &mut buf[..k]);
            w.write_all(&buf[..k]).await?;
            off += k;
        }
        w.flush().await?;
        w.shutdown().await?;
        anyhow::Ok(w)
    };
    let reader = async move {
        let mut got = Vec::with_capacity(len);
        r.read_to_end(&mut got).await?;
        anyhow::Ok(got)
    };
    let (w, got) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::try_join!(writer, reader)
    })
    .await
    .context("tunnel echo stalled")??;
    drop(w);
    payload::check(seed, len, &got).context("echo through the tunnel")
}

/// True when the peer ends the connection (EOF or reset) within `t`,
/// whatever it sent before (a TLS alert, an error page).
pub async fn ends_within<S: AsyncRead + Unpin>(s: &mut S, t: Duration) -> bool {
    let mut b = [0u8; 4096];
    tokio::time::timeout(t, async {
        loop {
            match s.read(&mut b).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await
    .is_ok()
}

/// True when the peer closes (EOF or reset) within `t` without sending
/// anything (bytes received count as "not closed").
pub async fn closed_within<S: AsyncRead + Unpin>(s: &mut S, t: Duration) -> bool {
    let mut b = [0u8; 256];
    matches!(
        tokio::time::timeout(t, s.read(&mut b)).await,
        Ok(Ok(0)) | Ok(Err(_))
    )
}
