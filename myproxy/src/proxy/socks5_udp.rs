//! SOCKS5 UDP ASSOCIATE (RFC 1928 §7): the relay behind QUIC/HTTP3, DNS
//! and games through the SOCKS5 listener. Separate path — the TCP relays
//! are untouched.
//!
//! - One UDP socket faces the client (bound on the IP the client reached
//!   us on); a second, dual-stack, faces the internet.
//! - Only datagrams from the TCP control connection's IP are relayed (the
//!   first one locks the client's port); replies are accepted only from
//!   destinations this association sent to — never an open inbound relay.
//! - Same destination policy as TCP (port allowlist, then the resolved
//!   address), names resolved through the DNS cache. Fragments (FRAG != 0)
//!   are dropped (optional in the RFC, unused in practice).
//! - The association lives as long as the TCP control connection, bounded
//!   by the idle timeout.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::AsyncReadExt;
use tokio::net::{TcpStream, UdpSocket};

use super::socks5::{reply, REP_OK};
use super::Services;
use crate::policy::canonical;

/// Room kept in front of a received reply for the SOCKS5 UDP header
/// (RSV 2 | FRAG 1 | ATYP 1 | IPv6 16 | PORT 2).
const HDR_MAX: usize = 22;
const DGRAM_MAX: usize = 65_535;
/// Destinations remembered per association (reply filter).
const MAX_PEERS: usize = 1024;

pub(super) async fn associate(mut ctl: TcpStream, peer: IpAddr, svc: &Services) -> std::io::Result<()> {
    let local_ip = ctl.local_addr()?.ip();
    let client_side = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?;
    let (remote_side, dual) = remote_socket()?;
    reply(&mut ctl, REP_OK, Some(client_side.local_addr()?)).await?;

    let peer = canonical(peer);
    let mut client_addr: Option<SocketAddr> = None;
    let mut sent_to: HashSet<SocketAddr> = HashSet::new();
    let mut up_buf = vec![0u8; DGRAM_MAX];
    let mut down_buf = vec![0u8; HDR_MAX + DGRAM_MAX];
    let mut probe = [0u8; 1];
    let idle = svc.cfg.timeouts.idle;
    loop {
        tokio::select! {
            // Control connection: EOF, error or any byte ends the association.
            _ = ctl.read(&mut probe) => return Ok(()),
            r = client_side.recv_from(&mut up_buf) => {
                let (n, from) = r?;
                if canonical(from.ip()) != peer || client_addr.is_some_and(|c| c != from) {
                    continue;
                }
                client_addr = Some(from);
                let Some((host, port, off)) = parse_header(&up_buf[..n]) else {
                    continue;
                };
                let Some(dest) = resolve(svc, &host, port).await else {
                    continue;
                };
                let to = if dual { v6_mapped(dest) } else { dest };
                if remote_side.send_to(&up_buf[off..n], to).await.is_ok()
                    && sent_to.len() < MAX_PEERS
                {
                    sent_to.insert(dest);
                }
            }
            r = remote_side.recv_from(&mut down_buf[HDR_MAX..]) => {
                let (n, from) = r?;
                let from = SocketAddr::new(canonical(from.ip()), from.port());
                let Some(client) = client_addr else { continue };
                if !sent_to.contains(&from) {
                    continue; // unsolicited: not a reply to this client
                }
                let start = write_header(&mut down_buf[..HDR_MAX], from);
                let _ = client_side.send_to(&down_buf[start..HDR_MAX + n], client).await;
            }
            _ = tokio::time::sleep(idle) => return Ok(()),
        }
    }
}

/// Internet-facing socket: dual-stack `[::]:0` (IPv4 as mapped addresses),
/// or plain IPv4 when IPv6 is unavailable. Returns (socket, dual-stack).
fn remote_socket() -> std::io::Result<(UdpSocket, bool)> {
    use socket2::{Domain, Protocol, Socket, Type};
    let dual = (|| {
        let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_only_v6(false)?;
        s.bind(&SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0).into())?;
        s.set_nonblocking(true)?;
        UdpSocket::from_std(s.into())
    })();
    match dual {
        Ok(s) => Ok((s, true)),
        Err(_) => {
            let s = std::net::UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))?;
            s.set_nonblocking(true)?;
            Ok((UdpSocket::from_std(s)?, false))
        }
    }
}

fn v6_mapped(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V4(v4) => SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port()),
        v6 => v6,
    }
}

/// RSV(2)=0 | FRAG | ATYP | DST.ADDR | DST.PORT | DATA -> (host, port, data offset).
fn parse_header(d: &[u8]) -> Option<(String, u16, usize)> {
    if d.len() < 4 || d[2] != 0 {
        return None; // truncated, or a fragment
    }
    let (host, at) = match d[3] {
        1 if d.len() >= 10 => (Ipv4Addr::new(d[4], d[5], d[6], d[7]).to_string(), 8),
        4 if d.len() >= 22 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&d[4..20]);
            (Ipv6Addr::from(a).to_string(), 20)
        }
        3 if d.len() >= 5 => {
            let len = d[4] as usize;
            if len == 0 || d.len() < 5 + len + 2 {
                return None;
            }
            (std::str::from_utf8(&d[5..5 + len]).ok()?.to_string(), 5 + len)
        }
        _ => return None,
    };
    let port = u16::from_be_bytes([d[at], d[at + 1]]);
    Some((host, port, at + 2))
}

/// Writes the header for `from` right before the data (which starts at
/// `buf.len()`); returns the header's start.
fn write_header(buf: &mut [u8], from: SocketAddr) -> usize {
    let end = buf.len();
    let port = from.port().to_be_bytes();
    match from.ip() {
        IpAddr::V4(a) => {
            let start = end - 10;
            buf[start..start + 4].copy_from_slice(&[0, 0, 0, 1]);
            buf[start + 4..start + 8].copy_from_slice(&a.octets());
            buf[start + 8..end].copy_from_slice(&port);
            start
        }
        IpAddr::V6(a) => {
            let start = end - 22;
            buf[start..start + 4].copy_from_slice(&[0, 0, 0, 4]);
            buf[start + 4..start + 20].copy_from_slice(&a.octets());
            buf[start + 20..end].copy_from_slice(&port);
            start
        }
    }
}

/// Destination policy + resolution (DNS cache) for one datagram.
async fn resolve(svc: &Services, host: &str, port: u16) -> Option<SocketAddr> {
    if !svc.dest.port_allowed(port) {
        return None;
    }
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => {
            let key = format!("{host}:{port}");
            match svc.dns.get(&key) {
                Some(a) => a,
                None => {
                    let v: Vec<SocketAddr> = tokio::time::timeout(
                        svc.cfg.timeouts.connect,
                        tokio::net::lookup_host((host, port)),
                    )
                    .await
                    .ok()?
                    .ok()?
                    .collect();
                    if v.is_empty() {
                        return None;
                    }
                    svc.dns.put(key, v.clone());
                    v
                }
            }
        }
    };
    addrs
        .into_iter()
        .map(|a| SocketAddr::new(canonical(a.ip()), a.port()))
        .find(|&a| svc.dest.addr_allowed(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let mut d = vec![0, 0, 0, 1, 1, 2, 3, 4, 0x01, 0xbb];
        d.extend_from_slice(b"payload");
        let (h, p, off) = parse_header(&d).unwrap();
        assert_eq!((h.as_str(), p, &d[off..]), ("1.2.3.4", 443, &b"payload"[..]));

        let mut d = vec![0, 0, 0, 3, 11];
        d.extend_from_slice(b"example.com");
        d.extend_from_slice(&[0, 53]);
        d.extend_from_slice(b"q");
        let (h, p, off) = parse_header(&d).unwrap();
        assert_eq!((h.as_str(), p, &d[off..]), ("example.com", 53, &b"q"[..]));

        assert!(parse_header(&[0, 0, 1, 1, 1, 2, 3, 4, 0, 1]).is_none()); // fragment
        assert!(parse_header(&[0, 0, 0, 3, 20, b'a']).is_none()); // truncated

        let mut buf = [0u8; HDR_MAX];
        let start = write_header(&mut buf, "5.6.7.8:9".parse().unwrap());
        assert_eq!(&buf[start..], &[0, 0, 0, 1, 5, 6, 7, 8, 0, 9]);
        let start = write_header(&mut buf, "[2001:db8::1]:53".parse().unwrap());
        assert_eq!(buf.len() - start, 22);
        assert_eq!(&buf[start..start + 4], &[0, 0, 0, 4]);
    }
}
