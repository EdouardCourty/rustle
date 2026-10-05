use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---- SOCKS5 UDP ASSOCIATE ----

/// TCP control connection + UDP ASSOCIATE; returns (control, relay addr).
async fn socks_udp(addr: SocketAddr) -> (TcpStream, SocketAddr) {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(&[5, 1, 0]).await.unwrap();
    let mut sel = [0u8; 2];
    c.read_exact(&mut sel).await.unwrap();
    c.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
    let mut rep = [0u8; 10];
    c.read_exact(&mut rep).await.unwrap();
    assert_eq!(rep[1], 0, "udp associate refused");
    let relay = SocketAddr::from((
        std::net::Ipv4Addr::new(rep[4], rep[5], rep[6], rep[7]),
        u16::from_be_bytes([rep[8], rep[9]]),
    ));
    (c, relay)
}

fn udp_dgram(to: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let SocketAddr::V4(t) = to else { panic!() };
    let mut d = vec![0, 0, 0, 1];
    d.extend_from_slice(&t.ip().octets());
    d.extend_from_slice(&t.port().to_be_bytes());
    d.extend_from_slice(payload);
    d
}

async fn udp_echo() -> SocketAddr {
    let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let a = s.local_addr().unwrap();
    tokio::spawn(async move {
        let mut b = [0u8; 2048];
        loop {
            let (n, from) = s.recv_from(&mut b).await.unwrap();
            let _ = s.send_to(&b[..n], from).await;
        }
    });
    a
}

#[tokio::test]
async fn socks5_udp_associate_relays_and_filters() {
    let echo = udp_echo().await;
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.socks5.listen = "127.0.0.1:0".into();
    let srv = start(Arc::new(cfg)).unwrap();
    let (ctl, relay) = socks_udp(srv.socks_addr().unwrap()).await;
    let u = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    u.send_to(&udp_dgram(echo, b"quic-ish"), relay)
        .await
        .unwrap();
    let mut b = [0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), u.recv_from(&mut b))
        .await
        .expect("no udp reply")
        .unwrap();
    // Reply carries the SOCKS5 header with the echo server as source.
    assert_eq!(&b[..n], &udp_dgram(echo, b"quic-ish")[..]);
    // Closing the control connection ends the association.
    drop(ctl);
    tokio::time::sleep(Duration::from_millis(50)).await;
    u.send_to(&udp_dgram(echo, b"late"), relay).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), u.recv_from(&mut b))
            .await
            .is_err()
    );
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();

    // Destination policy: a private target is dropped.
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.socks5.listen = "127.0.0.1:0".into();
    cfg.filter.block_private = true;
    let srv = start(Arc::new(cfg)).unwrap();
    let (ctl, relay) = socks_udp(srv.socks_addr().unwrap()).await;
    u.send_to(&udp_dgram(echo, b"x"), relay).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), u.recv_from(&mut b))
            .await
            .is_err()
    );
    drop(ctl);
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}
