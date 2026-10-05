//! Local listeners next to the proxy ports: the health endpoint and the
//! control socket.

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

use super::live::reload_from_file;
use super::Shared;

/// Health/admin listener: `GET /health` -> 200 "ok", anything else 404.
/// No auth, no proxying — separate port so probes never touch the proxy
/// path. 127.0.0.1-only by default on a public VPS.
pub(super) async fn admin_loop(listener: TcpListener) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((sock, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(async move {
            let mut sock = sock;
            let _ = sock.set_nodelay(true);
            // Bounded probe read: request line + headers, 4 KB max, 2 s.
            let mut buf = vec![0u8; 4096];
            let mut filled = 0usize;
            while filled < buf.len() {
                let read =
                    tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf[filled..]))
                        .await;
                match read {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return,
                    Ok(Ok(n)) => {
                        filled += n;
                        if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let head = String::from_utf8_lossy(&buf[..filled]);
            let ok = head.starts_with("GET /health");
            let body: &[u8] = if ok { b"ok" } else { b"" };
            let status = if ok { "200 OK" } else { "404 Not Found" };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            if ok {
                let _ = sock.write_all(body).await;
            }
        });
    }
}

/// Control socket: one command per connection, one line in, one line out
/// (`ok ...` or `error ...`). Commands: `reload`. Access = file permissions.
pub(super) async fn control_loop(listener: tokio::net::UnixListener, shared: Arc<Shared>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            let mut n = 0;
            while n < buf.len() && !buf[..n].contains(&b'\n') {
                match tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf[n..])).await {
                    Ok(Ok(k)) if k > 0 => n += k,
                    _ => return,
                }
            }
            let reply = match std::str::from_utf8(&buf[..n]).unwrap_or("").trim() {
                // File and certificate reads: off the shard's reactor.
                "reload" => {
                    match tokio::task::spawn_blocking(move || reload_from_file(&shared)).await {
                        Ok(Ok(r)) => {
                            tracing::info!(restart_required = ?r.restart_required, "config reloaded");
                            format!("ok {r}\n")
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(error = %format!("{e:#}"), "config reload failed, keeping the running config");
                            format!("error reload failed, running config kept: {e:#}\n")
                        }
                        Err(_) => "error reload panicked\n".to_string(),
                    }
                }
                _ => "error unknown command (known: reload)\n".to_string(),
            };
            let _ = sock.write_all(reply.as_bytes()).await;
        });
    }
}
