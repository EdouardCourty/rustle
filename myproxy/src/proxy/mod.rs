//! Proxy connection state machine: one task per client. Reads request
//! heads, checks auth, routes CONNECT (tunnel via [`relay`]) or plain
//! HTTP (via the [`pipeline`]) and loops for keep-alive.

mod pipeline;

use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::auth::Authenticator;
use crate::config::Config;
use crate::dns::DnsCache;
use crate::http::{classify, read_head, HeadError, Target};
use crate::relay;
use crate::upstream::UpstreamPool;

use pipeline::UpstreamError;

const RESP_407: &[u8] = b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"myproxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_400: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_431: &[u8] = b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_502: &[u8] =
    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_504: &[u8] =
    b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_200_CONNECT: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

pub async fn handle_conn(
    mut client: TcpStream,
    cfg: Arc<Config>,
    auth: Arc<Authenticator>,
    dns: Arc<DnsCache>,
    pool: Arc<UpstreamPool>,
) {
    let idle = cfg.timeouts.idle;
    let connect = cfg.timeouts.connect;
    let max_head = cfg.limits.max_header_bytes;

    // Grow-on-demand request buffer: an idle keep-alive connection parked in
    // read_head holds the vector's *capacity*, whose untouched pages cost
    // nothing; capacity is reused across requests on the same connection.
    let mut buf: Vec<u8> = Vec::new();
    // Connection-owned reusable relay pipe pair (created lazily on the
    // first bulk response, reused across keep-alive requests, dropped with
    // the connection).
    let mut resp_pipes: Option<relay::PipePair> = None;
    // M10.2: zero-alloc hot path — connection-owned scratches reused
    // across keep-alive requests (response head+body buffer, rewrite
    // buffer, origin key). Virtual capacity, dropped with the connection.
    let mut resp_buf: Vec<u8> = Vec::new();
    let mut rewrite_buf: Vec<u8> = Vec::new();
    let mut origin_key: String = String::new();
    // B: TCP_QUICKACK once per connection — the kernel re-arms it per
    // segment; per-request setsockopts were pure syscall overhead in the
    // keep-alive hot path (M10.3).
    client.set_quickack(true).ok();
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

        // M10.3: auth is a span into `buf` — sliced per check, zero alloc.
        if !auth.check(head.auth.map(|s| s.slice(&buf))) {
            let _ = client.write_all(RESP_407).await;
            return;
        }

        let target = match classify(&head, &buf) {
            Ok(t) => t,
            Err(_) => {
                let _ = client.write_all(RESP_400).await;
                return;
            }
        };

        match target {
            Target::Connect { host, port } => {
                // M10.3 fix: build the origin key from the CONNECT
                // authority — it used to stay empty, so every CONNECT
                // shared a single DNS cache entry.
                origin_key.clear();
                origin_key.push_str(&String::from_utf8_lossy(host.slice(&buf)));
                origin_key.push(':');
                pipeline::push_u16(&mut origin_key, port);
                // Preserve any early data the client sent after the CONNECT
                // head (spec-violating but seen in the wild).
                let early = buf[head.raw_len..].to_vec();
                let host_owned = String::from_utf8_lossy(host.slice(&buf)).into_owned();
                // Free the head-read buffer: the tunnel relay has its own
                // buffers, and holding this one would inflate RAM/connection.
                drop(std::mem::take(&mut buf));
                match pipeline::connect_upstream(&host_owned, port, connect, &dns, &origin_key)
                    .await
                {
                    Ok(mut up) => {
                        let _ = up.set_nodelay(true);
                        if client.write_all(RESP_200_CONNECT).await.is_err() {
                            return;
                        }
                        if !early.is_empty() {
                            let _ = timeout(idle, up.write_all(&early)).await;
                        }
                        // Tunnel: relay both directions until one side closes.
                        // Inactivity-bounded internally (no absolute cap).
                        match relay::bidirectional(client, up, idle).await {
                            Ok((c2u, u2c)) => {
                                tracing::trace!(c2u, u2c, "tunnel closed");
                            }
                            Err(e) => tracing::trace!(error = %e, "tunnel error"),
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
            Target::Http { .. } => {
                let keep_going = pipeline::http(
                    &mut client,
                    &cfg,
                    &mut buf,
                    &head,
                    &target,
                    &dns,
                    &pool,
                    &mut resp_pipes,
                    &mut resp_buf,
                    &mut rewrite_buf,
                    &mut origin_key,
                )
                .await;
                if !keep_going {
                    return;
                }
            }
        }
    }
}
