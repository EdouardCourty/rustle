//! Shared test infrastructure (no assertions here).

pub mod certs;
pub mod origin;
pub mod payload;
pub mod proxy;
pub mod targets;

use std::net::SocketAddr;
use std::sync::Arc;

pub use proxy::{Config, Proxy};

/// TCP listeners on 127.0.0.1 and ::1 sharing one port, so `localhost:port`
/// reaches the same service whatever the resolution order (::1 missing:
/// IPv4 only).
pub async fn tcp_pair() -> anyhow::Result<(tokio::net::TcpListener, Option<tokio::net::TcpListener>)>
{
    for _ in 0..20 {
        let l4 = listen("127.0.0.1:0".parse()?)?;
        let port = l4.local_addr()?.port();
        match listen(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))) {
            Ok(l6) => return Ok((l4, Some(l6))),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(_) => return Ok((l4, None)),
        }
    }
    anyhow::bail!("no port free on both 127.0.0.1 and ::1")
}

/// Listener with a deep accept queue (somaxconn caps it): 1 000 tunnels
/// opened at once while other scenarios run must not overflow the
/// targets' queue (dropped handshakes end as resets, not proxy bugs).
fn listen(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let s = if addr.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    s.bind(addr)?;
    s.listen(4096)
}

/// Same for UDP.
pub async fn udp_pair() -> anyhow::Result<(tokio::net::UdpSocket, Option<tokio::net::UdpSocket>)> {
    for _ in 0..20 {
        let s4 = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let port = s4.local_addr()?.port();
        match tokio::net::UdpSocket::bind(("::1", port)).await {
            Ok(s6) => return Ok((s4, Some(s6))),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(_) => return Ok((s4, None)),
        }
    }
    anyhow::bail!("no UDP port free on both 127.0.0.1 and ::1")
}

pub fn addr_of(l: &Option<tokio::net::TcpListener>) -> Option<SocketAddr> {
    l.as_ref().and_then(|l| l.local_addr().ok())
}

/// Everything a scenario can reach: local origins and targets, started
/// once for the whole run, and the host's capabilities.
pub struct Env {
    pub origin: origin::Origin,
    /// Second origin on another port (port allowlist tests).
    pub origin2: origin::Origin,
    pub targets: targets::Targets,
    pub ktls: bool,
}

impl Env {
    pub async fn start() -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            origin: origin::Origin::start().await?,
            origin2: origin::Origin::start().await?,
            targets: targets::Targets::start().await?,
            ktls: ktls_available(),
        }))
    }
}

/// Same probe as the proxy: can a TCP socket take the `tls` ULP?
fn ktls_available() -> bool {
    let probe = || -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let _c = std::net::TcpStream::connect(l.local_addr()?)?;
        let (s, _) = l.accept()?;
        let name = b"tls";
        // SAFETY: setsockopt on a valid fd with a valid buffer and length.
        let r = unsafe {
            libc::setsockopt(
                s.as_raw_fd(),
                libc::SOL_TCP,
                libc::TCP_ULP,
                name.as_ptr().cast(),
                name.len() as libc::socklen_t,
            )
        };
        if r == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    };
    probe().is_ok()
}
