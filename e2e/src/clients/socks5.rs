//! SOCKS5 client (RFC 1928 / 1929): CONNECT and UDP ASSOCIATE.

use std::net::{IpAddr, SocketAddr};

use anyhow::{bail, Context};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub enum Target {
    Ip(SocketAddr),
    Name(String, u16),
}

impl Target {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Target::Ip(SocketAddr::V4(a)) => {
                out.push(1);
                out.extend_from_slice(&a.ip().octets());
                out.extend_from_slice(&a.port().to_be_bytes());
            }
            Target::Ip(SocketAddr::V6(a)) => {
                out.push(4);
                out.extend_from_slice(&a.ip().octets());
                out.extend_from_slice(&a.port().to_be_bytes());
            }
            Target::Name(h, p) => {
                out.push(3);
                out.push(h.len() as u8);
                out.extend_from_slice(h.as_bytes());
                out.extend_from_slice(&p.to_be_bytes());
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Method negotiation refused (0xFF): credentials required.
    NoMethod,
    /// RFC 1929 status != 0.
    AuthFailed,
    /// Connection closed during the handshake.
    Closed,
    /// REP code (0 = success).
    Rep(u8),
}

/// Method selection + optional user/password. Ok(None) = proceed.
async fn hello(c: &mut TcpStream, auth: Option<(&str, &str)>) -> anyhow::Result<Option<Outcome>> {
    let method = if auth.is_some() { 2 } else { 0 };
    c.write_all(&[5, 1, method]).await?;
    let mut sel = [0u8; 2];
    if c.read_exact(&mut sel).await.is_err() {
        return Ok(Some(Outcome::Closed));
    }
    if sel[1] == 0xff {
        return Ok(Some(Outcome::NoMethod));
    }
    anyhow::ensure!(sel == [5, method], "unexpected method selection {sel:?}");
    if let Some((u, p)) = auth {
        let mut m = vec![1, u.len() as u8];
        m.extend_from_slice(u.as_bytes());
        m.push(p.len() as u8);
        m.extend_from_slice(p.as_bytes());
        c.write_all(&m).await?;
        let mut st = [0u8; 2];
        if c.read_exact(&mut st).await.is_err() {
            return Ok(Some(Outcome::Closed));
        }
        if st[1] != 0 {
            return Ok(Some(Outcome::AuthFailed));
        }
    }
    Ok(None)
}

/// Reads a reply: (REP, bound address).
async fn reply(c: &mut TcpStream) -> anyhow::Result<Option<(u8, SocketAddr)>> {
    let mut h = [0u8; 4];
    if c.read_exact(&mut h).await.is_err() {
        return Ok(None);
    }
    let ip: IpAddr = match h[3] {
        1 => {
            let mut a = [0u8; 4];
            c.read_exact(&mut a).await?;
            a.into()
        }
        4 => {
            let mut a = [0u8; 16];
            c.read_exact(&mut a).await?;
            a.into()
        }
        t => bail!("unexpected ATYP {t} in reply"),
    };
    let mut p = [0u8; 2];
    c.read_exact(&mut p).await?;
    Ok(Some((h[1], SocketAddr::new(ip, u16::from_be_bytes(p)))))
}

/// CONNECT; on `Rep(0)` the stream is the tunnel.
pub async fn connect(
    proxy: SocketAddr,
    auth: Option<(&str, &str)>,
    target: &Target,
) -> anyhow::Result<(TcpStream, Outcome)> {
    let mut c = TcpStream::connect(proxy)
        .await
        .context("connect to SOCKS5")?;
    c.set_nodelay(true)?;
    if let Some(o) = hello(&mut c, auth).await? {
        return Ok((c, o));
    }
    let mut req = vec![5, 1, 0];
    target.encode(&mut req);
    c.write_all(&req).await?;
    let o = match reply(&mut c).await? {
        Some((rep, _)) => Outcome::Rep(rep),
        None => Outcome::Closed,
    };
    Ok((c, o))
}

/// UDP ASSOCIATE; returns (control connection, relay address).
pub async fn udp_associate(
    proxy: SocketAddr,
    auth: Option<(&str, &str)>,
) -> anyhow::Result<(TcpStream, SocketAddr)> {
    let mut c = TcpStream::connect(proxy).await?;
    if let Some(o) = hello(&mut c, auth).await? {
        bail!("UDP ASSOCIATE handshake: {o:?}");
    }
    c.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    let (rep, mut relay) = reply(&mut c).await?.context("closed after UDP ASSOCIATE")?;
    anyhow::ensure!(rep == 0, "UDP ASSOCIATE refused: REP {rep}");
    if relay.ip().is_unspecified() {
        relay.set_ip(proxy.ip());
    }
    Ok((c, relay))
}

pub fn udp_packet(target: &Target, payload: &[u8]) -> Vec<u8> {
    let mut d = vec![0, 0, 0];
    target.encode(&mut d);
    d.extend_from_slice(payload);
    d
}

/// Splits a relayed datagram: (source, payload).
pub fn udp_parse(d: &[u8]) -> anyhow::Result<(SocketAddr, &[u8])> {
    anyhow::ensure!(d.len() > 4 && d[2] == 0, "bad UDP header");
    let (ip, rest): (IpAddr, &[u8]) = match d[3] {
        1 => (<[u8; 4]>::try_from(&d[4..8])?.into(), &d[8..]),
        4 => (<[u8; 16]>::try_from(&d[4..20])?.into(), &d[20..]),
        t => bail!("ATYP {t} in relayed datagram"),
    };
    let port = u16::from_be_bytes([rest[0], rest[1]]);
    Ok((SocketAddr::new(ip, port), &rest[2..]))
}
