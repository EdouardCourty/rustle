use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::auth::Authenticator;
use crate::config::Config;
use crate::http::{classify, read_head, rewrite_head, HeadError, Target};

const RESP_407: &[u8] = b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"myproxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_400: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_431: &[u8] = b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_502: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_504: &[u8] = b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_200_CONNECT: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

enum UpstreamError {
    Timeout,
    Other(std::io::Error),
}

pub async fn handle_conn(mut client: TcpStream, cfg: Arc<Config>, auth: Arc<Authenticator>) {
    let idle = cfg.timeouts.idle;
    let connect = cfg.timeouts.connect;
    let max_head = cfg.limits.max_header_bytes;

    let mut buf = Vec::with_capacity(8192);
    loop {
        let head = match read_head(&mut client, &mut buf, max_head, idle).await {
            Ok(Some(h)) => h,
            Ok(None) => return,
            Err(HeadError::Io(e)) => {
                tracing::trace!(error = %e, "client read error");
                return;
            }
            Err(HeadError::TooLarge) => {
                let _ = client.write_all(RESP_431).await;
                return;
            }
            Err(HeadError::Invalid) => {
                let _ = client.write_all(RESP_400).await;
                return;
            }
        };

        if !auth.check(head.auth.as_deref()) {
            let _ = client.write_all(RESP_407).await;
            return;
        }

        let target = match classify(&head) {
            Ok(t) => t,
            Err(_) => {
                let _ = client.write_all(RESP_400).await;
                return;
            }
        };

        match target {
            Target::Connect { host, port } => {
                match connect_upstream(&host, port, connect).await {
                    Ok(mut up) => {
                        let _ = up.set_nodelay(true);
                        if client.write_all(RESP_200_CONNECT).await.is_err() {
                            return;
                        }
                        // Tunnel: relay both directions until one side closes.
                        match timeout(idle, tokio::io::copy_bidirectional(&mut client, &mut up))
                            .await
                        {
                            Ok(Ok((c2u, u2c))) => {
                                tracing::trace!(c2u, u2c, "tunnel closed");
                            }
                            Ok(Err(e)) => tracing::trace!(error = %e, "tunnel error"),
                            Err(_) => tracing::trace!("tunnel idle timeout"),
                        }
                    }
                    Err(UpstreamError::Timeout) => {
                        let _ = client.write_all(RESP_504).await;
                    }
                    Err(UpstreamError::Other(e)) => {
                        tracing::debug!(error = %e, "upstream connect failed");
                        let _ = client.write_all(RESP_502).await;
                    }
                }
                return;
            }
            Target::Http { host, port, origin } => {
                let mut up = match connect_upstream(&host, port, connect).await {
                    Ok(s) => s,
                    Err(UpstreamError::Timeout) => {
                        let _ = client.write_all(RESP_504).await;
                        return;
                    }
                    Err(UpstreamError::Other(e)) => {
                        tracing::debug!(error = %e, "upstream connect failed");
                        let _ = client.write_all(RESP_502).await;
                        return;
                    }
                };
                let _ = up.set_nodelay(true);

                if !forward_request(&mut client, &mut up, &buf, &head, &origin, idle).await {
                    return;
                }
                // Response relay: upstream is Connection: close, so the response
                // ends at EOF. Then loop for the next keep-alive request.
                let _ = timeout(idle, tokio::io::copy(&mut up, &mut client)).await;
                if head.close || head.version_minor == 0 {
                    return;
                }
            }
        }
    }
}

/// Writes the rewritten request head plus the request body to the upstream.
/// Returns false when the client connection must be dropped.
async fn forward_request(
    client: &mut TcpStream,
    up: &mut TcpStream,
    buf: &[u8],
    head: &crate::http::RequestHead,
    origin: &str,
    idle: Duration,
) -> bool {
    let rewritten = rewrite_head(&buf[..head.raw_len], &head.method, origin);
    if timeout(idle, up.write_all(&rewritten)).await.is_err() {
        return false;
    }

    let content_length = head.content_length.unwrap_or(0);
    let body = &buf[head.raw_len..];
    if body.len() as u64 > content_length {
        // Pipelined bytes beyond the body are not supported in V1.
        return false;
    }
    if !body.is_empty() {
        if timeout(idle, up.write_all(body)).await.is_err() {
            return false;
        }
    }
    let remaining = content_length - body.len() as u64;
    if remaining > 0 {
        let mut limited = (&mut *client).take(remaining);
        if timeout(idle, tokio::io::copy(&mut limited, up)).await.is_err() {
            return false;
        }
    }
    true
}

async fn connect_upstream(
    host: &str,
    port: u16,
    connect: Duration,
) -> Result<TcpStream, UpstreamError> {
    match timeout(connect, TcpStream::connect((host, port))).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(UpstreamError::Other(e)),
        Err(_) => Err(UpstreamError::Timeout),
    }
}
