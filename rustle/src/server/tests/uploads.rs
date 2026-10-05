use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---- request bodies without Content-Length (chunked uploads) ----

/// Origin that reads each request body (Content-Length or chunked)
/// and answers "got <n>" (n = decoded body bytes).
async fn upload_origin() -> SocketAddr {
    let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = ln.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = ln.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut r = tokio::io::BufReader::new(s);
                use tokio::io::AsyncBufReadExt;
                loop {
                    let mut head = String::new();
                    loop {
                        let mut l = String::new();
                        if r.read_line(&mut l).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if l == "\r\n" {
                            break;
                        }
                        head.push_str(&l.to_ascii_lowercase());
                    }
                    let mut n = 0usize;
                    if head.contains("transfer-encoding: chunked") {
                        loop {
                            let mut l = String::new();
                            r.read_line(&mut l).await.unwrap();
                            let size = usize::from_str_radix(l.trim(), 16).unwrap();
                            if size == 0 {
                                r.read_line(&mut l).await.unwrap(); // final CRLF
                                break;
                            }
                            let mut data = vec![0u8; size + 2];
                            r.read_exact(&mut data).await.unwrap();
                            n += size;
                        }
                    } else if let Some(cl) = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                    {
                        let cl: usize = cl.trim().parse().unwrap();
                        let mut data = vec![0u8; cl];
                        r.read_exact(&mut data).await.unwrap();
                        n = cl;
                    }
                    let body = format!("got {n}");
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if r.get_mut().write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn chunked_upload_http1() {
    let up = upload_origin().await;
    let srv = start(test_config(1, 100)).unwrap();
    let mut c = TcpStream::connect(srv.local_addr()).await.unwrap();
    // Head and chunks in separate writes (framing split across reads).
    c.write_all(format!("POST http://{up}/u HTTP/1.1\r\nHost: {up}\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel").as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    c.write_all(b"lo\r\n6\r\n world\r\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    c.write_all(b"0\r\n\r\n").await.unwrap();
    let (st, body) = read_resp(&mut c).await;
    assert!(st.starts_with("HTTP/1.1 200"), "{st}");
    assert_eq!(body, b"got 11");
    // Keep-alive survives: a Content-Length upload on the same connection.
    c.write_all(
        format!("POST http://{up}/u HTTP/1.1\r\nHost: {up}\r\nContent-Length: 3\r\n\r\nabc")
            .as_bytes(),
    )
    .await
    .unwrap();
    let (st, body) = read_resp(&mut c).await;
    assert!(st.starts_with("HTTP/1.1 200"), "{st}");
    assert_eq!(body, b"got 3");
    drop(c);
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn chunked_upload_over_budget_is_cut() {
    let up = upload_origin().await;
    let mut cfg = (*test_config(1, 100)).clone();
    cfg.http.max_request_body_bytes = 8;
    let srv = start(Arc::new(cfg)).unwrap();
    let out = send_recv(
        srv.local_addr(),
        format!("POST http://{up}/u HTTP/1.1\r\nHost: {up}\r\nTransfer-Encoding: chunked\r\n\r\n10\r\n0123456789abcdef\r\n0\r\n\r\n").as_bytes(),
    )
    .await;
    assert!(
        !status(&out).starts_with("HTTP/1.1 200"),
        "{}",
        status(&out)
    );
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn h2_upload_without_length() {
    let (cfg, der) = tls_config();
    let up = upload_origin().await;
    let srv = start(cfg).unwrap();
    let mut send = h2_client(srv.tls_addr().unwrap(), der).await;
    let req = http::Request::post(format!("http://{up}/u"))
        .body(())
        .unwrap();
    let (resp, mut tx) = send.send_request(req, false).unwrap();
    tx.send_data(bytes::Bytes::from_static(b"hello "), false)
        .unwrap();
    tx.send_data(bytes::Bytes::from_static(b"h2 world"), true)
        .unwrap();
    let resp = resp.await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(h2_body(resp.into_body()).await, b"got 14");
    drop((tx, send));
    tokio::task::spawn_blocking(move || srv.shutdown_and_join())
        .await
        .unwrap()
        .unwrap();
}
