//! Raw targets for tunnels: TCP echo (half-close aware), a server that
//! speaks first, a TLS echo (real end-to-end TLS through CONNECT) and UDP
//! echo. Each listens on 127.0.0.1 and, when available, ::1.

use std::net::SocketAddr;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

use super::certs;

pub struct Targets {
    /// Echoes everything, then half-closes after the client's EOF.
    pub echo: SocketAddr,
    pub echo6: Option<SocketAddr>,
    /// Writes `HELLO\r\n` first, then echoes.
    pub greeter: SocketAddr,
    /// TLS server (certificate for localhost) that echoes the plaintext.
    pub tls_echo: SocketAddr,
    /// HTTPS origin (certificate for localhost): `GET /data/<n>` answers
    /// payload `n` of `n` bytes, keep-alive.
    pub https: SocketAddr,
    pub udp: SocketAddr,
    pub udp6: Option<SocketAddr>,
}

impl Targets {
    pub async fn start() -> anyhow::Result<Self> {
        let (echo, echo6) = tcp(Kind::Echo).await?;
        let (greeter, _) = tcp(Kind::Greeter).await?;
        let (tls_echo, _) = tcp(Kind::Tls).await?;
        let (https, _) = tcp(Kind::Https).await?;
        let (s4, s6) = super::udp_pair().await?;
        let udp = s4.local_addr()?;
        let udp6 = s6.as_ref().and_then(|s| s.local_addr().ok());
        udp_echo(s4);
        if let Some(s6) = s6 {
            udp_echo(s6);
        }
        Ok(Self {
            echo,
            echo6,
            greeter,
            tls_echo,
            https,
            udp,
            udp6,
        })
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Echo,
    Greeter,
    Tls,
    Https,
}

/// Serves `kind` on 127.0.0.1 and ::1 (same port).
async fn tcp(kind: Kind) -> anyhow::Result<(SocketAddr, Option<SocketAddr>)> {
    let (l4, l6) = super::tcp_pair().await?;
    let a = (l4.local_addr()?, super::addr_of(&l6));
    serve(l4, kind);
    if let Some(l6) = l6 {
        serve(l6, kind);
    }
    Ok(a)
}

fn serve(l: TcpListener, kind: Kind) {
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                match kind {
                    Kind::Echo => echo(s).await,
                    Kind::Greeter => {
                        let mut s = s;
                        if s.write_all(b"HELLO\r\n").await.is_ok() {
                            echo(s).await
                        }
                    }
                    Kind::Tls => {
                        let acc = tokio_rustls::TlsAcceptor::from(certs::server());
                        if let Ok(t) = acc.accept(s).await {
                            echo(t).await
                        }
                    }
                    Kind::Https => {
                        let acc = tokio_rustls::TlsAcceptor::from(certs::server());
                        if let Ok(t) = acc.accept(s).await {
                            https_data(t).await
                        }
                    }
                }
            });
        }
    });
}

/// Echo until EOF, then flush and shut down the write side.
async fn echo<S: AsyncRead + AsyncWrite + Unpin>(mut s: S) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match s.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if s.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
        }
    }
    let _ = s.shutdown().await;
}

/// Minimal HTTPS origin: `/data/<n>` only, Content-Length, keep-alive.
async fn https_data<S: AsyncRead + AsyncWrite + Unpin>(s: S) {
    let mut w = super::origin::Wire::new(s);
    while let Ok(Some(head)) = w.head().await {
        let (first, _) = super::origin::parse_headers(&head);
        let n: usize = first
            .split(' ')
            .nth(1)
            .and_then(|p| p.strip_prefix("/data/"))
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let mut resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {n}\r\n\r\n").into_bytes();
        resp.extend(super::payload::make(n as u64, n));
        if w.s.write_all(&resp).await.is_err() || w.s.flush().await.is_err() {
            return;
        }
    }
}

fn udp_echo(s: UdpSocket) {
    tokio::spawn(async move {
        let mut b = vec![0u8; 65536];
        loop {
            if let Ok((n, from)) = s.recv_from(&mut b).await {
                let _ = s.send_to(&b[..n], from).await;
            }
        }
    });
}
