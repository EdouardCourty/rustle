//! SOCKS5 listener (RFC 1928): CONNECT and UDP ASSOCIATE (QUIC/HTTP3,
//! DNS, games); BIND is refused with "command not supported". Authentication is RFC 1929
//! username/password with the API key as the password (the username is
//! ignored, as with HTTP Basic); no authentication when no key is
//! configured. Same destination policy and ban rules as the HTTP side; the
//! tunnel is the same zero-copy relay as HTTP CONNECT.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::pipeline::{self, UpstreamError};
use super::Services;
use crate::relay;

const VER: u8 = 5;
const NO_AUTH: u8 = 0x00;
const USER_PASS: u8 = 0x02;
const NO_METHOD: u8 = 0xff;

/// Reply codes (RFC 1928 §6).
pub(super) const REP_OK: u8 = 0x00;
const REP_FAILURE: u8 = 0x01;
const REP_NOT_ALLOWED: u8 = 0x02;
const REP_HOST_UNREACHABLE: u8 = 0x04;
const REP_REFUSED: u8 = 0x05;
const REP_TTL_EXPIRED: u8 = 0x06;
const REP_CMD_UNSUPPORTED: u8 = 0x07;
const REP_ATYP_UNSUPPORTED: u8 = 0x08;

/// What the client asked for.
enum Cmd {
    Connect(String, u16),
    UdpAssociate,
}

pub async fn handle_socks5(mut client: TcpStream, peer: IpAddr, svc: Arc<Services>) {
    let cfg = &svc.cfg;
    // The whole negotiation is bounded (slowloris), like a request head.
    let cmd = match timeout(cfg.timeouts.request_head, negotiate(&mut client, peer, &svc)).await {
        Ok(Ok(Some(c))) => c,
        _ => return,
    };
    let (host, port) = match cmd {
        Cmd::Connect(h, p) => (h, p),
        Cmd::UdpAssociate => {
            if let Err(e) = super::socks5_udp::associate(client, peer, &svc).await {
                tracing::debug!(error = %e, "socks5 udp association ended");
            }
            return;
        }
    };
    let mut key = String::with_capacity(host.len() + 6);
    key.push_str(&host);
    key.push(':');
    pipeline::push_u16(&mut key, port);
    let up = match pipeline::connect_upstream(&host, port, cfg.timeouts.connect, &svc.dns, &key, &svc.dest).await {
        Ok(up) => up,
        Err(e) => {
            // Access log speaks HTTP status codes for every transport.
            let (rep, status, reason) = match e {
                UpstreamError::Forbidden(r) => (REP_NOT_ALLOWED, 403, Some(r)),
                UpstreamError::Timeout => (REP_TTL_EXPIRED, 504, None),
                UpstreamError::Other(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    (REP_REFUSED, 502, Some("refused"))
                }
                UpstreamError::Other(_) => (REP_HOST_UNREACHABLE, 502, None),
            };
            svc.log_fail(peer, "socks5", "connect", &key, status, reason);
            let _ = reply(&mut client, rep, None).await;
            return;
        }
    };
    let _ = up.set_nodelay(true);
    if reply(&mut client, REP_OK, up.local_addr().ok()).await.is_err() {
        return;
    }
    let t0 = std::time::Instant::now();
    match relay::bidirectional(client, up, cfg.timeouts.idle).await {
        Ok((c2u, u2c)) => {
            tracing::trace!(c2u, u2c, "socks5 tunnel closed");
            if svc.log.all() {
                svc.log_tunnel(peer, "socks5", "connect", &key, (c2u, u2c), t0);
            }
        }
        Err(e) => tracing::trace!(error = %e, "socks5 tunnel error"),
    }
}

/// Method selection, authentication and the request. `None` = refused
/// (the client already got its reply).
async fn negotiate(
    c: &mut TcpStream,
    peer: IpAddr,
    svc: &Services,
) -> std::io::Result<Option<Cmd>> {
    let mut h = [0u8; 2];
    c.read_exact(&mut h).await?;
    if h[0] != VER {
        return Ok(None);
    }
    let mut methods = [0u8; 255];
    let methods = &mut methods[..h[1] as usize];
    c.read_exact(methods).await?;
    let want = if svc.auth.enabled() { USER_PASS } else { NO_AUTH };
    if !methods.contains(&want) {
        c.write_all(&[VER, NO_METHOD]).await?;
        return Ok(None);
    }
    c.write_all(&[VER, want]).await?;

    if want == USER_PASS {
        // RFC 1929: VER=1 | ULEN | UNAME | PLEN | PASSWD
        let mut v = [0u8; 2];
        c.read_exact(&mut v).await?;
        if v[0] != 1 {
            return Ok(None);
        }
        let mut field = [0u8; 255];
        c.read_exact(&mut field[..v[1] as usize]).await?; // username: ignored
        let mut plen = [0u8; 1];
        c.read_exact(&mut plen).await?;
        let pass = &mut field[..plen[0] as usize];
        c.read_exact(pass).await?;
        let ok = svc.auth.check_key(pass);
        c.write_all(&[1, if ok { 0 } else { 1 }]).await?;
        if !ok {
            svc.auth_failed(peer, "socks5");
            return Ok(None);
        }
    }

    // Request: VER | CMD | RSV | ATYP | DST.ADDR | DST.PORT
    let mut r = [0u8; 4];
    c.read_exact(&mut r).await?;
    if r[0] != VER {
        return Ok(None);
    }
    let host = match r[3] {
        1 => {
            let mut a = [0u8; 4];
            c.read_exact(&mut a).await?;
            Ipv4Addr::from(a).to_string()
        }
        3 => {
            let mut len = [0u8; 1];
            c.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            c.read_exact(&mut name).await?;
            match String::from_utf8(name) {
                Ok(n) if !n.is_empty() => n,
                _ => {
                    reply(c, REP_FAILURE, None).await?;
                    return Ok(None);
                }
            }
        }
        4 => {
            let mut a = [0u8; 16];
            c.read_exact(&mut a).await?;
            Ipv6Addr::from(a).to_string()
        }
        _ => {
            reply(c, REP_ATYP_UNSUPPORTED, None).await?;
            return Ok(None);
        }
    };
    let mut p = [0u8; 2];
    c.read_exact(&mut p).await?;
    match r[1] {
        1 => Ok(Some(Cmd::Connect(host, u16::from_be_bytes(p)))),
        // DST of an UDP ASSOCIATE is the client's own (often zero) address:
        // datagrams are accepted from the TCP peer's IP only.
        3 => Ok(Some(Cmd::UdpAssociate)),
        _ => {
            reply(c, REP_CMD_UNSUPPORTED, None).await?;
            Ok(None)
        }
    }
}

/// VER | REP | RSV | ATYP | BND.ADDR | BND.PORT
pub(super) async fn reply(c: &mut TcpStream, rep: u8, bound: Option<SocketAddr>) -> std::io::Result<()> {
    let mut out = [0u8; 22];
    out[..3].copy_from_slice(&[VER, rep, 0]);
    let n = match bound {
        Some(SocketAddr::V6(a)) => {
            out[3] = 4;
            out[4..20].copy_from_slice(&a.ip().octets());
            out[20..22].copy_from_slice(&a.port().to_be_bytes());
            22
        }
        Some(SocketAddr::V4(a)) => {
            out[3] = 1;
            out[4..8].copy_from_slice(&a.ip().octets());
            out[8..10].copy_from_slice(&a.port().to_be_bytes());
            10
        }
        None => {
            out[3] = 1; // 0.0.0.0:0
            10
        }
    };
    c.write_all(&out[..n]).await
}
