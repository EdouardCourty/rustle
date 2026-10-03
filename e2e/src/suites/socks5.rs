//! SOCKS5: method negotiation, RFC 1929 auth, address types, unsupported
//! commands, UDP ASSOCIATE (filtering, source check, lifetime).

use std::time::Duration;

use anyhow::ensure;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

use crate::clients::echo_roundtrip;
use crate::clients::socks5::{self, Outcome, Target};
use crate::harness::proxy::{KEY, KEY2};
use crate::harness::{Config, Proxy};
use crate::runner::{scenario, skip, Scenario};

/// One UDP exchange through the relay; Some(payload) when echoed.
async fn udp_once(
    sock: &UdpSocket,
    relay: std::net::SocketAddr,
    target: &Target,
    data: &[u8],
) -> anyhow::Result<Option<Vec<u8>>> {
    let pkt = socks5::udp_packet(target, data);
    let mut buf = vec![0u8; 70000];
    for _ in 0..3 {
        sock.send_to(&pkt, relay).await?;
        if let Ok(r) =
            tokio::time::timeout(Duration::from_millis(700), sock.recv_from(&mut buf)).await
        {
            let (n, _) = r?;
            return Ok(Some(socks5::udp_parse(&buf[..n])?.1.to_vec()));
        }
    }
    Ok(None)
}

pub fn scenarios() -> Vec<Scenario> {
    vec![
        scenario(
            "socks5/auth/method-without-credentials-refused",
            |env| async move {
                let p = Proxy::start(Config::new()).await?;
                let (_s, o) =
                    socks5::connect(p.socks(), None, &Target::Ip(env.targets.echo)).await?;
                ensure!(o == Outcome::NoMethod, "{o:?}");
                p.finish().await
            },
        )
        .quick(),
        scenario("socks5/auth/wrong-key", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let (_s, o) = socks5::connect(
                p.socks(),
                Some(("u", "nope")),
                &Target::Ip(env.targets.echo),
            )
            .await?;
            ensure!(o == Outcome::AuthFailed, "{o:?}");
            p.finish().await
        })
        .quick(),
        scenario("socks5/auth/any-user-second-key", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let (s, o) = socks5::connect(
                p.socks(),
                Some(("whatever-user", KEY2)),
                &Target::Ip(env.targets.echo),
            )
            .await?;
            ensure!(o == Outcome::Rep(0), "{o:?}");
            echo_roundtrip(s, 1, 1000).await?;
            p.finish().await
        }),
        scenario("socks5/auth/open-relay-no-auth", |env| async move {
            let p = Proxy::start(Config::new().set("auth", "api_keys", "[]")).await?;
            let (s, o) = socks5::connect(p.socks(), None, &Target::Ip(env.targets.echo)).await?;
            ensure!(o == Outcome::Rep(0), "{o:?}");
            echo_roundtrip(s, 1, 1000).await?;
            p.finish().await
        }),
        scenario("socks5/address/domain", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let t = Target::Name("localhost".into(), env.targets.echo.port());
            let (s, o) = socks5::connect(p.socks(), Some(("u", KEY)), &t).await?;
            ensure!(o == Outcome::Rep(0), "{o:?}");
            echo_roundtrip(s, 2, 50_000).await?;
            p.finish().await
        }),
        scenario("socks5/command/bind-unsupported", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = TcpStream::connect(p.socks()).await?;
            c.write_all(&[5, 1, 2]).await?;
            let mut sel = [0u8; 2];
            c.read_exact(&mut sel).await?;
            let mut m = vec![1, 1, b'u', KEY.len() as u8];
            m.extend_from_slice(KEY.as_bytes());
            c.write_all(&m).await?;
            c.read_exact(&mut sel).await?;
            let mut req = vec![5, 2, 0, 1, 127, 0, 0, 1];
            req.extend_from_slice(&env.targets.echo.port().to_be_bytes());
            c.write_all(&req).await?;
            let mut rep = [0u8; 4];
            c.read_exact(&mut rep).await?;
            ensure!(rep[1] == 7, "BIND: REP {}", rep[1]);
            drop(c);
            p.finish().await
        }),
        scenario("socks5/protocol/wrong-version-closed", |_env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = TcpStream::connect(p.socks()).await?;
            c.write_all(&[4, 1, 0, 80, 127, 0, 0, 1, 0]).await?;
            ensure!(
                crate::clients::closed_within(&mut c, Duration::from_secs(3)).await,
                "SOCKS4 not closed"
            );
            drop(c);
            p.finish().await
        }),
        scenario("socks5/udp/ipv4-domain-ipv6", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let (_ctl, relay) = socks5::udp_associate(p.socks(), Some(("u", KEY))).await?;
            let sock = UdpSocket::bind("127.0.0.1:0").await?;
            let mut targets = vec![
                Target::Ip(env.targets.udp),
                Target::Name("localhost".into(), env.targets.udp.port()),
            ];
            if let Some(a) = env.targets.udp6 {
                targets.push(Target::Ip(a));
            }
            for (i, t) in targets.iter().enumerate() {
                let data = format!("datagram {i}");
                let got = udp_once(&sock, relay, t, data.as_bytes()).await?;
                ensure!(
                    got.as_deref() == Some(data.as_bytes()),
                    "target #{i}: {got:?}"
                );
            }
            p.finish().await
        })
        .quick(),
        scenario("socks5/udp/policy-drops-forbidden-port", |env| async move {
            let cfg = Config::new().set(
                "filter",
                "allowed_ports",
                &format!("[{}]", env.targets.udp.port()),
            );
            let p = Proxy::start(cfg).await?;
            let (_ctl, relay) = socks5::udp_associate(p.socks(), Some(("u", KEY))).await?;
            let sock = UdpSocket::bind("127.0.0.1:0").await?;
            // Another UDP echo port, not allowed.
            let other = UdpSocket::bind("127.0.0.1:0").await?;
            let other_addr = other.local_addr()?;
            tokio::spawn(async move {
                let mut b = [0u8; 2048];
                while let Ok((n, from)) = other.recv_from(&mut b).await {
                    let _ = other.send_to(&b[..n], from).await;
                }
            });
            ensure!(
                udp_once(&sock, relay, &Target::Ip(other_addr), b"x")
                    .await?
                    .is_none(),
                "forbidden port relayed"
            );
            ensure!(
                udp_once(&sock, relay, &Target::Ip(env.targets.udp), b"y")
                    .await?
                    .as_deref()
                    == Some(&b"y"[..])
            );
            p.finish().await
        }),
        scenario("socks5/udp/foreign-source-ignored", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let (_ctl, relay) = socks5::udp_associate(p.socks(), Some(("u", KEY))).await?;
            // The control connection comes from 127.0.0.1: a datagram from
            // 127.0.0.2 is someone else.
            let Ok(stranger) = UdpSocket::bind("127.0.0.2:0").await else {
                return Err(skip("cannot bind 127.0.0.2"));
            };
            ensure!(
                udp_once(&stranger, relay, &Target::Ip(env.targets.udp), b"x")
                    .await?
                    .is_none(),
                "datagram from a foreign IP relayed"
            );
            p.finish().await
        }),
        scenario(
            "socks5/udp/association-ends-with-control-connection",
            |env| async move {
                let p = Proxy::start(Config::new()).await?;
                let (ctl, relay) = socks5::udp_associate(p.socks(), Some(("u", KEY))).await?;
                let sock = UdpSocket::bind("127.0.0.1:0").await?;
                let t = Target::Ip(env.targets.udp);
                ensure!(udp_once(&sock, relay, &t, b"a").await?.is_some());
                drop(ctl);
                tokio::time::sleep(Duration::from_millis(300)).await;
                ensure!(
                    udp_once(&sock, relay, &t, b"b").await?.is_none(),
                    "relay alive after its control connection closed"
                );
                p.finish().await
            },
        ),
        scenario("socks5/udp/many-datagrams", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            let (_ctl, relay) = socks5::udp_associate(p.socks(), Some(("u", KEY))).await?;
            let sock = UdpSocket::bind("127.0.0.1:0").await?;
            let t = Target::Ip(env.targets.udp);
            let mut ok = 0;
            for i in 0..500u32 {
                let pkt = socks5::udp_packet(&t, &i.to_be_bytes());
                sock.send_to(&pkt, relay).await?;
                let mut b = [0u8; 64];
                if let Ok(Ok((n, _))) =
                    tokio::time::timeout(Duration::from_millis(500), sock.recv_from(&mut b)).await
                {
                    if socks5::udp_parse(&b[..n])?.1 == i.to_be_bytes() {
                        ok += 1;
                    }
                }
            }
            // Loopback UDP: losses would be a proxy bug, allow a handful.
            ensure!(ok >= 495, "{ok}/500 datagrams echoed");
            p.finish().await
        }),
    ]
}
