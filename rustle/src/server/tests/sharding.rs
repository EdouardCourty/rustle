use super::*;

/// The CPU steering program is accepted by the kernel and a group
/// with it still accepts connections (on any CPU: unknown ones fall
/// back to the hash).
#[cfg(target_os = "linux")]
#[test]
fn cpu_steering_attaches() {
    use std::io::Write;
    let a = bind("127.0.0.1:0".parse().unwrap(), true).unwrap();
    let addr = a.local_addr().unwrap();
    let b = bind(addr, true).unwrap();
    steer_by_cpu(&a, &[0, 1]).unwrap();
    // A byte each: TCP_DEFER_ACCEPT holds silent connections back.
    let clients: Vec<_> = (0..8)
        .map(|_| {
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            c.write_all(b"x").unwrap();
            c
        })
        .collect();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got = 0;
    while got < clients.len() && std::time::Instant::now() < deadline {
        let before = got;
        got += [&a, &b].iter().filter(|l| l.accept().is_ok()).count();
        if got == before {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert_eq!(got, clients.len());
}

/// The per-connection setsockopts were dropped on the promise that
/// accepted sockets inherit them from the listener: keep it checked.
#[tokio::test]
async fn accepted_socket_inherits_listener_options() {
    let ln = TcpListener::from_std(bind("127.0.0.1:0".parse().unwrap(), false).unwrap()).unwrap();
    let addr = ln.local_addr().unwrap();
    let _c = TcpStream::connect(addr).await.unwrap();
    let (s, _) = ln.accept().await.unwrap();
    assert!(s.nodelay().unwrap());
    // Compared with the listener, not with the requested size: the
    // kernel caps it at net.core.{r,w}mem_max (208 KB on CI runners).
    let l = socket2::SockRef::from(&ln);
    let s = socket2::SockRef::from(&s);
    assert!(s.send_buffer_size().unwrap() >= l.send_buffer_size().unwrap());
    assert!(s.recv_buffer_size().unwrap() >= l.recv_buffer_size().unwrap());
}

#[tokio::test]
async fn shards_share_the_port_and_split_the_load() {
    let srv = start(test_config(2, 1000)).unwrap();
    let addr = srv.local_addr();
    for _ in 0..64 {
        let status = roundtrip(addr, b"garbage\r\n\r\n").await;
        assert!(status.starts_with("HTTP/1.1 400"), "{status}");
    }
    let per_shard: Vec<u64> = srv
        .shared
        .accepted
        .iter()
        .map(|a| a.0.load(Relaxed))
        .collect();
    assert_eq!(per_shard.iter().sum::<u64>(), 64);
    // Linux only: elsewhere the shards share one listener and the kernel
    // may hand every connection to the same one.
    #[cfg(target_os = "linux")]
    assert!(
        per_shard.iter().all(|&n| n > 0),
        "unbalanced: {per_shard:?}"
    );
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn max_connections_is_global_across_shards() {
    let srv = start(test_config(2, 2)).unwrap();
    let addr = srv.local_addr();
    // Two idle connections hold both slots (on whichever shards).
    let _a = TcpStream::connect(addr).await.unwrap();
    let _b = TcpStream::connect(addr).await.unwrap();
    while srv.shared.active.load(Relaxed) < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let status = roundtrip(addr, b"").await;
    assert!(status.starts_with("HTTP/1.1 503"), "{status}");
    drop((_a, _b));
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}
