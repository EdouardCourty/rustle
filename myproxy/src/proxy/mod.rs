//! Proxy connection state machine: one task per client. Reads request
//! heads, checks auth, routes CONNECT (tunnel via [`relay`]) or plain
//! HTTP (via the [`pipeline`]) and loops for keep-alive. Generic over the
//! client transport ([`ClientStream`]: plain TCP or TLS).

mod h2;
mod pipeline;
mod socks5;
mod socks5_udp;

pub use h2::handle_h2;
pub use socks5::handle_socks5;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::accesslog::AccessLog;
use crate::auth::Authenticator;
use crate::config::Config;
use crate::dns::DnsCache;
use crate::http::{classify, read_head, HeadError, Target};
use crate::policy::{AuthLimiter, DestPolicy};
use crate::relay;
use crate::stream::ClientStream;
use crate::upstream::UpstreamPool;

use pipeline::UpstreamError;

/// What a connection needs, one `Arc` per shard (one refcount bump per
/// connection): the upstream pool is the shard's own, the rest is shared
/// process-wide.
pub struct Services {
    pub cfg: Arc<Config>,
    pub auth: Arc<Authenticator>,
    pub dns: Arc<DnsCache>,
    pub dest: Arc<DestPolicy>,
    pub limiter: Arc<AuthLimiter>,
    pub log: Arc<AccessLog>,
    pub pool: UpstreamPool,
}

impl Services {
    /// Failure line (errors level): refused or failed request/tunnel.
    pub(crate) fn log_fail(
        &self,
        peer: IpAddr,
        via: &str,
        kind: &str,
        target: &str,
        status: u16,
        reason: Option<&str>,
    ) {
        if !self.log.errors() {
            return;
        }
        let mut l = self
            .log
            .line()
            .kv("client", peer)
            .kv("via", via)
            .kv("kind", kind)
            .kv("target", target)
            .kv("status", status);
        if let Some(r) = reason {
            l = l.kv("reason", r);
        }
        self.log.emit(l);
    }

    /// Wrong credentials: counts towards a ban; logs the failure and the
    /// ban when it triggers.
    pub(crate) fn auth_failed(&self, peer: IpAddr, via: &str) {
        let banned = self.limiter.fail(peer);
        if !self.log.errors() {
            return;
        }
        self.log.emit(
            self.log
                .line()
                .kv("client", peer)
                .kv("event", "auth-fail")
                .kv("via", via),
        );
        if banned {
            self.log.emit(
                self.log
                    .line()
                    .kv("client", peer)
                    .kv("event", "ban")
                    .kv("failures", self.cfg.auth.max_failures)
                    .kv(
                        "for",
                        format_args!("{}s", self.cfg.auth.ban_duration.as_secs()),
                    ),
            );
        }
    }

    /// Tunnel closed (all level).
    pub(crate) fn log_tunnel(
        &self,
        peer: IpAddr,
        via: &str,
        kind: &str,
        target: &str,
        (up, down): (u64, u64),
        t0: std::time::Instant,
    ) {
        self.log.emit(
            self.log
                .line()
                .kv("client", peer)
                .kv("via", via)
                .kv("kind", kind)
                .kv("target", target)
                .kv("status", 200)
                .kv("up", up)
                .kv("down", down)
                .kv("ms", t0.elapsed().as_millis()),
        );
    }
}

/// A connection that became a CONNECT tunnel.
struct Tunnel {
    up: TcpStream,
    /// `host:port`, kept only when every tunnel is logged.
    target: Option<String>,
    t0: std::time::Instant,
}

const RESP_407: &[u8] = b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"myproxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_403: &[u8] = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
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

pub async fn handle_conn<C: ClientStream>(mut client: C, peer: IpAddr, svc: Arc<Services>) {
    let idle = svc.cfg.timeouts.idle;
    match serve(&mut client, peer, &svc).await {
        // Tunnel: relay both directions until one side closes.
        // Inactivity-bounded internally (no absolute cap).
        Some(t) => match relay::bidirectional(client, t.up, idle).await {
            Ok((c2u, u2c)) => {
                tracing::trace!(c2u, u2c, "tunnel closed");
                if let Some(target) = t.target {
                    svc.log_tunnel(peer, C::VIA, "connect", &target, (c2u, u2c), t.t0);
                }
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
async fn serve<C: ClientStream>(client: &mut C, peer: IpAddr, svc: &Services) -> Option<Tunnel> {
    let cfg: &Config = &svc.cfg;
    let idle = cfg.timeouts.idle;
    let head_timeout = cfg.timeouts.request_head;
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
        let head = match read_head(client, &mut buf, max_head, idle, head_timeout).await {
            Ok(Some(h)) => h,
            Ok(None) => return None,
            Err(HeadError::Io(e)) => {
                tracing::trace!(error = %e, "client read error");
                return None;
            }
            Err(HeadError::TooLarge) => {
                svc.log_fail(peer, C::VIA, "http", "-", 431, None);
                reply(client, RESP_431).await;
                return None;
            }
            Err(HeadError::Invalid) => {
                svc.log_fail(peer, C::VIA, "http", "-", 400, None);
                reply(client, RESP_400).await;
                return None;
            }
        };

        // Proxy-Authorization checked on every request (keep-alive safe).
        if !svc.auth.check(head.auth.map(|s| s.slice(&buf))) {
            // Wrong credentials count towards a ban; none at all is the
            // normal first challenge of a browser.
            if head.auth.is_some() {
                svc.auth_failed(peer, C::VIA);
            }
            reply(client, RESP_407).await;
            return None;
        }

        let target = match classify(&head, &buf) {
            Ok(t) => t,
            Err(_) => {
                svc.log_fail(peer, C::VIA, "http", "-", 400, None);
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
                    &svc.dns,
                    &origin_key,
                    &svc.dest,
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
                        Some(Tunnel {
                            up,
                            target: svc.log.all().then(|| origin_key.clone()),
                            t0: std::time::Instant::now(),
                        })
                    }
                    Err(UpstreamError::Timeout) => {
                        svc.log_fail(peer, C::VIA, "connect", &origin_key, 504, None);
                        reply(client, RESP_504).await;
                        None
                    }
                    Err(UpstreamError::Forbidden(reason)) => {
                        svc.log_fail(peer, C::VIA, "connect", &origin_key, 403, Some(reason));
                        reply(client, RESP_403).await;
                        None
                    }
                    Err(UpstreamError::Other(e)) => {
                        tracing::debug!(error = %e, "upstream connect failed");
                        svc.log_fail(peer, C::VIA, "connect", &origin_key, 502, None);
                        reply(client, RESP_502).await;
                        None
                    }
                };
            }
            Target::Http { .. } => {
                let keep_going = pipeline::http(
                    client,
                    svc,
                    peer,
                    &mut buf,
                    &head,
                    &target,
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
