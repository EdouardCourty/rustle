use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(target_os = "linux")]
#[tokio::test]
async fn https_proxy_serves_keepalive_http_and_chunked() {
    let (cfg, der) = tls_config();
    let mut cfg = (*cfg).clone();
    cfg.tls.ktls = true; // opt-in; this box has the `tls` module
    let up = origin().await;
    let srv = start(Arc::new(cfg)).unwrap();
    let mut c = tls_connect(srv.tls_addr().unwrap(), der).await;
    https_keepalive_round(&mut c, up).await;
    // The kernel took the session (module `tls` present on this box).
    assert_eq!(srv.ktls_sessions(), 1, "kTLS not engaged");
    drop(c);
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn https_proxy_userspace_tls_when_ktls_off() {
    let (cfg, der) = tls_config();
    let mut cfg = (*cfg).clone();
    cfg.tls.ktls = false;
    let up = origin().await;
    let srv = start(Arc::new(cfg)).unwrap();
    let mut c = tls_connect(srv.tls_addr().unwrap(), der).await;
    https_keepalive_round(&mut c, up).await;
    assert_eq!(srv.ktls_sessions(), 0);
    drop(c);
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}

/// Keep-alive GETs (CL + chunked) and a flushed 400 over one TLS session.
async fn https_keepalive_round<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    c: &mut S,
    up: SocketAddr,
) {
    for path in ["/a", "/chunked", "/b"] {
        let req = format!("GET http://{up}{path} HTTP/1.1\r\nHost: {up}\r\n\r\n");
        c.write_all(req.as_bytes()).await.unwrap();
        let (status, body) = read_resp(c).await;
        assert!(status.starts_with("HTTP/1.1 200"), "{path}: {status}");
        if path == "/chunked" {
            assert_eq!(body, b"5\r\nhello\r\n0\r\n\r\n");
        } else {
            assert_eq!(body.len(), 1000);
        }
    }
    // Errors are flushed through TLS too (not stuck in the session).
    c.write_all(b"garbage\r\n\r\n").await.unwrap();
    let (status, _) = read_resp(c).await;
    assert!(status.starts_with("HTTP/1.1 400"), "{status}");
}

#[tokio::test]
async fn https_proxy_connect_tunnel() {
    let (cfg, der) = tls_config();
    // Echo server as the CONNECT target.
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = echo.accept().await.unwrap();
        let (mut r, mut w) = s.split();
        let _ = tokio::io::copy(&mut r, &mut w).await;
    });
    let srv = start(cfg).unwrap();
    let mut c = tls_connect(srv.tls_addr().unwrap(), der).await;
    let req = format!("CONNECT {echo_addr} HTTP/1.1\r\nHost: {echo_addr}\r\n\r\n");
    c.write_all(req.as_bytes()).await.unwrap();
    let mut head = vec![0u8; 39];
    c.read_exact(&mut head).await.unwrap();
    assert!(
        head.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&head)
    );
    let payload: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
    let (mut r, mut w) = tokio::io::split(c);
    let p2 = payload.clone();
    let writer = tokio::spawn(async move {
        w.write_all(&p2).await.unwrap();
        w.flush().await.unwrap();
        w
    });
    let mut got = vec![0u8; payload.len()];
    r.read_exact(&mut got).await.unwrap();
    assert_eq!(got, payload);
    drop((r, writer.await.unwrap()));
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}
