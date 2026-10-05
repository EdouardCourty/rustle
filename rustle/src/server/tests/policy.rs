use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn destination_policy_and_port_allowlist() {
    let up = origin().await;
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.filter.block_private = true;
    let srv = start(Arc::new(cfg)).unwrap();
    let a = srv.local_addr();
    // Private destination (numeric and by name), CONNECT and plain GET.
    for req in [
        format!("CONNECT {up} HTTP/1.1\r\nHost: {up}\r\n\r\n"),
        format!("GET http://{up}/a HTTP/1.1\r\nHost: {up}\r\n\r\n"),
        format!(
            "GET http://localhost:{}/a HTTP/1.1\r\nHost: x\r\n\r\n",
            up.port()
        ),
        "CONNECT 169.254.169.254:80 HTTP/1.1\r\nHost: x\r\n\r\n".to_string(),
    ] {
        let st = status(&send_recv(a, req.as_bytes()).await);
        assert!(st.starts_with("HTTP/1.1 403"), "{req}: {st}");
    }
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();

    // Port allowlist (private filter off): allowed port passes, other refused.
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.filter.allowed_ports = vec![up.port()];
    let srv = start(Arc::new(cfg)).unwrap();
    let a = srv.local_addr();
    let ok = send_recv(
        a,
        format!("GET http://{up}/a HTTP/1.1\r\nHost: {up}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await;
    assert!(status(&ok).starts_with("HTTP/1.1 200"), "{}", status(&ok));
    let st = status(&send_recv(a, b"CONNECT 127.0.0.1:1 HTTP/1.1\r\nHost: x\r\n\r\n").await);
    assert!(st.starts_with("HTTP/1.1 403"), "{st}");
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn wrong_keys_ban_the_client_but_missing_credentials_do_not() {
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.auth.api_keys = vec!["good".into()];
    cfg.auth.max_failures = 3;
    let srv = start(Arc::new(cfg)).unwrap();
    let a = srv.local_addr();
    use base64::Engine as _;
    let bad = base64::engine::general_purpose::STANDARD.encode("u:bad");
    // No credentials: plain 407, never counted.
    for _ in 0..5 {
        let st = status(&send_recv(a, b"CONNECT x:1 HTTP/1.1\r\nHost: x\r\n\r\n").await);
        assert!(st.starts_with("HTTP/1.1 407"), "{st}");
    }
    for _ in 0..3 {
        let req = format!("CONNECT x:1 HTTP/1.1\r\nProxy-Authorization: Basic {bad}\r\n\r\n");
        let st = status(&send_recv(a, req.as_bytes()).await);
        assert!(st.starts_with("HTTP/1.1 407"), "{st}");
    }
    // Banned: the connection is closed without a response, even with
    // the good key.
    let good = base64::engine::general_purpose::STANDARD.encode("u:good");
    let req = format!("CONNECT x:1 HTTP/1.1\r\nProxy-Authorization: Basic {good}\r\n\r\n");
    assert!(send_recv(a, req.as_bytes()).await.is_empty());
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn slowloris_head_is_cut() {
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.timeouts.request_head = Duration::from_millis(300);
    let srv = start(Arc::new(cfg)).unwrap();
    let mut c = TcpStream::connect(srv.local_addr()).await.unwrap();
    let t0 = std::time::Instant::now();
    // One byte every 100 ms: each read is quick, the head never ends.
    let mut closed = false;
    for b in b"GET http://example.com/ HTTP/1.1\r\nX-Slow: aaaaaaaaaaaaaaaaaaaaaaaa".iter() {
        if c.write_all(&[*b]).await.is_err() {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut probe = [0u8; 1];
        if let Ok(Ok(0)) = tokio::time::timeout(Duration::from_millis(1), c.read(&mut probe)).await
        {
            closed = true;
            break;
        }
    }
    assert!(closed, "trickling client never cut");
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn socks5_auth_tunnel_and_policy() {
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = echo.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.auth.api_keys = vec!["k1".into()];
    cfg.socks5.listen = "127.0.0.1:0".into();
    let srv = start(Arc::new(cfg)).unwrap();
    let sa = srv.socks_addr().unwrap();

    let (_, rep) = socks_connect(sa, Some("nope"), echo_addr).await;
    assert_eq!(rep, 0xff, "wrong key accepted");

    let (mut c, rep) = socks_connect(sa, Some("k1"), echo_addr).await;
    assert_eq!(rep, 0);
    c.write_all(b"ping through socks").await.unwrap();
    let mut back = [0u8; 18];
    c.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"ping through socks");
    drop(c);
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();

    // Destination policy applies to SOCKS5 too (REP 0x02).
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.filter.block_private = true;
    cfg.socks5.listen = "127.0.0.1:0".into();
    let srv = start(Arc::new(cfg)).unwrap();
    let (_, rep) = socks_connect(srv.socks_addr().unwrap(), None, echo_addr).await;
    assert_eq!(rep, 0x02);
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn client_allowlist_closes_outsiders_at_accept() {
    // 127.0.0.1 outside the list: closed without a response, on the
    // HTTP and the SOCKS5 listeners.
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.auth.allow_clients = vec!["10.0.0.0/8".into()];
    cfg.socks5.listen = "127.0.0.1:0".into();
    let srv = start(Arc::new(cfg)).unwrap();
    assert!(send_recv(srv.local_addr(), b"garbage\r\n\r\n")
        .await
        .is_empty());
    assert!(send_recv(srv.socks_addr().unwrap(), &[5, 1, 0])
        .await
        .is_empty());
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();

    // Inside the list: served as usual.
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.auth.allow_clients = vec!["127.0.0.0/8".into()];
    let srv = start(Arc::new(cfg)).unwrap();
    let st = status(&send_recv(srv.local_addr(), b"garbage\r\n\r\n").await);
    assert!(st.starts_with("HTTP/1.1 400"), "{st}");
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}
