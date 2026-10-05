use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---- access log ----

fn log_path(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rustle-access-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("access.log")
}

/// Waits (writer flushes every 200 ms) until the file has a line
/// containing every needle; returns the file content.
async fn wait_log(path: &std::path::Path, needles: &[&str]) -> String {
    for _ in 0..40 {
        let s = std::fs::read_to_string(path).unwrap_or_default();
        if s.lines().any(|l| needles.iter().all(|n| l.contains(n))) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "no line with {needles:?} in {}:\n{}",
        path.display(),
        std::fs::read_to_string(path).unwrap_or_default()
    );
}

#[tokio::test]
async fn access_log_all_requests_and_tunnels() {
    let up = origin().await;
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
    let path = log_path("all");
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.log.access = crate::accesslog::Level::All;
    cfg.log.file = path.to_string_lossy().into_owned();
    cfg.log.target = crate::accesslog::TargetMode::Url;
    cfg.socks5.listen = "127.0.0.1:0".into();
    let srv = start(Arc::new(cfg)).unwrap();

    let out = send_recv(
        srv.local_addr(),
        format!("GET http://{up}/page?q=1 HTTP/1.1\r\nHost: {up}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
    .await;
    assert!(status(&out).starts_with("HTTP/1.1 200"));
    wait_log(
        &path,
        &[
            "via=http1",
            "kind=http",
            "method=GET",
            &format!("target=http://{up}/page?q=1"),
            "status=200",
            "down=1000",
        ],
    )
    .await;

    let mut c = TcpStream::connect(srv.local_addr()).await.unwrap();
    c.write_all(format!("CONNECT {echo_addr} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut head = vec![0u8; 39];
    c.read_exact(&mut head).await.unwrap();
    c.write_all(b"12345").await.unwrap();
    let mut back = [0u8; 5];
    c.read_exact(&mut back).await.unwrap();
    drop(c);
    wait_log(
        &path,
        &[
            "kind=connect",
            &format!("target={echo_addr}"),
            "up=5",
            "down=5",
        ],
    )
    .await;

    let (mut c, rep) = socks_connect(srv.socks_addr().unwrap(), None, echo_addr).await;
    assert_eq!(rep, 0);
    c.write_all(b"abc").await.unwrap();
    let mut back = [0u8; 3];
    c.read_exact(&mut back).await.unwrap();
    drop(c);
    wait_log(&path, &["via=socks5", "kind=connect", "up=3", "down=3"]).await;
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn access_log_errors_only_and_reopen() {
    let up = origin().await;
    let path = log_path("errors");
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.log.access = crate::accesslog::Level::Errors;
    cfg.log.file = path.to_string_lossy().into_owned();
    cfg.auth.api_keys = vec!["k".into()];
    cfg.filter.allowed_ports = vec![up.port()];
    let srv = start(Arc::new(cfg)).unwrap();
    let a = srv.local_addr();
    use base64::Engine as _;
    let good = base64::engine::general_purpose::STANDARD.encode("u:k");
    let bad = base64::engine::general_purpose::STANDARD.encode("u:x");

    // A 200 is not logged at `errors`.
    let ok = send_recv(a, format!("GET http://{up}/a HTTP/1.1\r\nProxy-Authorization: Basic {good}\r\nConnection: close\r\n\r\n").as_bytes()).await;
    assert!(status(&ok).starts_with("HTTP/1.1 200"));
    // Port refused and wrong key are.
    send_recv(
        a,
        format!("CONNECT 127.0.0.1:1 HTTP/1.1\r\nProxy-Authorization: Basic {good}\r\n\r\n")
            .as_bytes(),
    )
    .await;
    send_recv(
        a,
        format!("CONNECT x:1 HTTP/1.1\r\nProxy-Authorization: Basic {bad}\r\n\r\n").as_bytes(),
    )
    .await;
    let s = wait_log(&path, &["event=auth-fail"]).await;
    assert!(s.contains("status=403 reason=port"), "{s}");
    assert!(!s.contains("status=200"), "{s}");

    // logrotate: rename, reopen, the next line lands in the new file.
    let rotated = path.with_extension("log.1");
    std::fs::rename(&path, &rotated).unwrap();
    srv.reopen_logs();
    tokio::time::sleep(Duration::from_millis(300)).await;
    send_recv(a, b"CONNECT 127.0.0.1:2 HTTP/1.1\r\n\r\n").await; // 407 without creds: not logged
    send_recv(
        a,
        format!("CONNECT 127.0.0.1:2 HTTP/1.1\r\nProxy-Authorization: Basic {good}\r\n\r\n")
            .as_bytes(),
    )
    .await;
    wait_log(&path, &["target=127.0.0.1:2", "status=403"]).await;
    assert!(!std::fs::read_to_string(&rotated)
        .unwrap()
        .contains("127.0.0.1:2"));
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}
