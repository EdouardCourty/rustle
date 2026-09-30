//! Proxy connection state machine: one task per client. Reads request
//! heads, checks auth, routes CONNECT (tunnel via [`relay`]) or plain
//! HTTP (via the [`pipeline`]) and loops for keep-alive. Generic over the
//! client transport ([`ClientStream`]: plain TCP or TLS).

mod h2;
mod pipeline;

pub use h2::handle_h2;

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::auth::Authenticator;
use crate::config::Config;
use crate::dns::DnsCache;
use crate::http::{classify, read_head, HeadError, Target};
use crate::relay;
use crate::stream::ClientStream;
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

/// Final reply before the connection is dropped (error responses).
async fn reply<C: ClientStream>(client: &mut C, resp: &[u8]) {
    let _ = relay::write_flush(client, resp).await;
}

pub async fn handle_conn<C: ClientStream>(
    mut client: C,
    cfg: Arc<Config>,
    auth: Arc<Authenticator>,
    dns: Arc<DnsCache>,
    pool: Arc<UpstreamPool>,
) {
    let idle = cfg.timeouts.idle;
    match serve(&mut client, &cfg, &auth, &dns, &pool).await {
        // Tunnel: relay both directions until one side closes.
        // Inactivity-bounded internally (no absolute cap).
        Some(up) => match relay::bidirectional(client, up, idle).await {
            Ok((c2u, u2c)) => {
                tracing::trace!(c2u, u2c, "tunnel closed");
            }
            Err(e) => tracing::trace!(error = %e, "tunnel error"),
        },
        None => {
            if C::TLS {
                // close_notify: a TLS peer must see a clean end of stream.
                let _ = timeout(Duration::from_secs(1), client.shutdown()).await;
            }
        }
    }
}

/// The keep-alive loop. Returns the upstream socket when the connection
/// becomes a CONNECT tunnel (the 200 is already sent); `None` when it must
/// be closed.
async fn serve<C: ClientStream>(
    client: &mut C,
    cfg: &Config,
    auth: &Authenticator,
    dns: &DnsCache,
    pool: &UpstreamPool,
) -> Option<TcpStream> {
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
    // Zero-alloc hot path — connection-owned scratches reused
    // across keep-alive requests (response head+body buffer, rewrite
    // buffer, origin key). Virtual capacity, dropped with the connection.
    let mut resp_buf: Vec<u8> = Vec::new();
    let mut rewrite_buf: Vec<u8> = Vec::new();
    let mut origin_key: String = String::new();
    // B: TCP_QUICKACK once per connection — the kernel re-arms it per
    // segment; per-request setsockopts were pure syscall overhead in the
    // keep-alive hot path.
    if let Some(tcp) = client.tcp() {
        tcp.set_quickack(true).ok();
    }
    loop {
        let head = match read_head(client, &mut buf, max_head, idle).await {
            Ok(Some(h)) => h,
            Ok(None) => return None,
            Err(HeadError::Io(e)) => {
                tracing::trace!(error = %e, "client read error");
                return None;
            }
            Err(HeadError::TooLarge) => {
                reply(client, RESP_431).await;
                return None;
            }
            Err(HeadError::Invalid) => {
                reply(client, RESP_400).await;
                return None;
            }
        };

        // Proxy-Authorization checked on every request (keep-alive safe).
        if !auth.check(head.auth.map(|s| s.slice(&buf))) {
            reply(client, RESP_407).await;
            return None;
        }

        let target = match classify(&head, &buf) {
            Ok(t) => t,
            Err(_) => {
                reply(client, RESP_400).await;
                return None;
            }
        };

        match target {
            Target::Connect { host, port } => {
                // Build the origin key from the CONNECT
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
                return match pipeline::connect_upstream(
                    &host_owned,
                    port,
                    connect,
                    dns,
                    &origin_key,
                )
                .await
                {
                    Ok(mut up) => {
                        let _ = up.set_nodelay(true);
                        if relay::write_flush(client, RESP_200_CONNECT).await.is_err() {
                            return None;
                        }
                        if !early.is_empty() {
                            let _ = timeout(idle, up.write_all(&early)).await;
                        }
                        Some(up)
                    }
                    Err(UpstreamError::Timeout) => {
                        reply(client, RESP_504).await;
                        None
                    }
                    Err(UpstreamError::Other(e)) => {
                        tracing::debug!(error = %e, "upstream connect failed");
                        reply(client, RESP_502).await;
                        None
                    }
                };
            }
            Target::Http { .. } => {
                let keep_going = pipeline::http(
                    client,
                    cfg,
                    &mut buf,
                    &head,
                    &target,
                    dns,
                    pool,
                    &mut resp_pipes,
                    &mut resp_buf,
                    &mut rewrite_buf,
                    &mut origin_key,
                )
                .await;
                if !keep_going {
                    return None;
                }
            }
        }
    }
}
