use super::*;

#[tokio::test]
async fn h2_get_content_length_and_chunked() {
    let (cfg, der) = tls_config();
    let up = origin().await;
    let srv = start(cfg).unwrap();
    let mut send = h2_client(srv.tls_addr().unwrap(), der).await;
    // Several streams on one connection, including a decoded chunked body.
    for (path, want) in [
        ("/a", vec![b'x'; 1000]),
        ("/chunked", b"hello".to_vec()),
        ("/b", vec![b'x'; 1000]),
    ] {
        let req = http::Request::get(format!("http://{up}{path}"))
            .body(())
            .unwrap();
        let (resp, _) = send.send_request(req, true).unwrap();
        let resp = resp.await.unwrap();
        assert_eq!(resp.status(), 200, "{path}");
        assert!(resp.headers().get("transfer-encoding").is_none());
        assert_eq!(h2_body(resp.into_body()).await, want, "{path}");
    }
    drop(send);
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn h2_connect_tunnel_and_auth() {
    let (cfg, der) = tls_config();
    let mut cfg = (*cfg).clone();
    cfg.auth.api_keys = vec!["k1".to_string()];
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = echo.accept().await.unwrap();
        let (mut r, mut w) = s.split();
        let _ = tokio::io::copy(&mut r, &mut w).await;
    });
    let srv = start(Arc::new(cfg)).unwrap();
    let mut send = h2_client(srv.tls_addr().unwrap(), der).await;

    // No credentials: 407 on the stream, the connection survives.
    let req = http::Request::builder()
        .method(http::Method::CONNECT)
        .uri(echo_addr.to_string())
        .body(())
        .unwrap();
    let (resp, _) = send.send_request(req, true).unwrap();
    assert_eq!(resp.await.unwrap().status(), 407);

    use base64::Engine as _;
    let cred = base64::engine::general_purpose::STANDARD.encode("u:k1");
    let req = http::Request::builder()
        .method(http::Method::CONNECT)
        .uri(echo_addr.to_string())
        .header("proxy-authorization", format!("Basic {cred}"))
        .body(())
        .unwrap();
    let (resp, mut tx) = send.send_request(req, false).unwrap();
    let resp = resp.await.unwrap();
    assert_eq!(resp.status(), 200);
    let mut rx = resp.into_body();
    let payload: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
    let p2 = payload.clone();
    let writer = tokio::spawn(async move {
        let mut data = bytes::Bytes::from(p2);
        while !data.is_empty() {
            tx.reserve_capacity(data.len());
            let cap = std::future::poll_fn(|cx| tx.poll_capacity(cx))
                .await
                .unwrap()
                .unwrap();
            let chunk = data.split_to(cap.min(data.len()));
            tx.send_data(chunk, false).unwrap();
        }
        tx.send_data(bytes::Bytes::new(), true).unwrap();
    });
    let mut got = Vec::new();
    while got.len() < payload.len() {
        let chunk = rx.data().await.unwrap().unwrap();
        let _ = rx.flow_control().release_capacity(chunk.len());
        got.extend_from_slice(&chunk);
    }
    assert_eq!(got, payload);
    writer.await.unwrap();
    drop((rx, send));
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}
