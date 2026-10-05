//! WebSocket handshakes on the plain-HTTP path (`GET` + `Connection:
//! Upgrade` + `Upgrade: websocket`): forwarded on a fresh upstream
//! connection; when the origin answers 101 the connection becomes a tunnel
//! (spliced like CONNECT), otherwise the origin's answer is relayed and the
//! connection closes. Gated by `http.allow_websocket`.

use std::net::IpAddr;

use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

use crate::http::{
    read_response_head, rewrite_head_into, HeadError, RequestHead, RespError, Target,
};
use crate::relay;
use crate::stream::ClientStream;

use super::pipeline::{connect_upstream, push_u16, UpstreamError};
use super::{reply, Services, Tunnel, RESP_400, RESP_403, RESP_502, RESP_504};

#[allow(clippy::too_many_arguments)]
pub(super) async fn websocket<C: ClientStream>(
    client: &mut C,
    peer: IpAddr,
    svc: &Services,
    buf: &mut [u8],
    head: &RequestHead,
    target: &Target,
    resp_buf: &mut Vec<u8>,
    rewrite_buf: &mut Vec<u8>,
) -> Option<Tunnel> {
    let Target::Http { host, port, origin } = target else {
        return None;
    };
    let cfg = &svc.cfg;
    let (idle, max_head) = (cfg.timeouts.idle, cfg.limits.max_header_bytes);
    let host_str = String::from_utf8_lossy(host.slice(buf)).into_owned();
    let mut key = host_str.clone();
    key.push(':');
    push_u16(&mut key, *port);
    let fail = |status: u16, reason: Option<&str>| {
        svc.log_fail(peer, C::VIA, "websocket", &key, status, reason)
    };

    if !cfg.http.allow_websocket {
        fail(403, Some("websocket-disabled"));
        reply(client, RESP_403).await;
        return None;
    }
    // A handshake is a bodyless GET.
    if head.chunked || head.content_length.unwrap_or(0) > 0 {
        fail(400, Some("upgrade-body"));
        reply(client, RESP_400).await;
        return None;
    }

    // Always a fresh connection: an upgraded socket never returns to the pool.
    let mut up = match connect_upstream(
        &host_str,
        *port,
        cfg.timeouts.connect,
        &svc.dns,
        &key,
        &svc.dest,
    )
    .await
    {
        Ok(s) => s,
        Err(UpstreamError::Forbidden(reason)) => {
            fail(403, Some(reason));
            reply(client, RESP_403).await;
            return None;
        }
        Err(UpstreamError::Timeout) => {
            fail(504, None);
            reply(client, RESP_504).await;
            return None;
        }
        Err(UpstreamError::Other(e)) => {
            tracing::debug!(error = %e, "upstream connect failed");
            fail(502, None);
            reply(client, RESP_502).await;
            return None;
        }
    };
    let _ = up.set_nodelay(true);

    rewrite_head_into(
        rewrite_buf,
        &buf[..head.raw_len],
        head.method.slice(buf),
        origin.slice(buf),
        true,
    );
    // Bytes the client sent right behind its handshake: for the origin, once
    // it accepted.
    let early = buf[head.raw_len..].to_vec();
    if !matches!(timeout(idle, up.write_all(rewrite_buf)).await, Ok(Ok(()))) {
        fail(502, Some("upstream-write"));
        reply(client, RESP_502).await;
        return None;
    }

    let rhead = match read_response_head(&mut up, resp_buf, max_head, idle, &[]).await {
        Ok(h) => h,
        Err(RespError::Head(HeadError::Io(e))) if e.kind() == std::io::ErrorKind::TimedOut => {
            fail(504, Some("no-response"));
            reply(client, RESP_504).await;
            return None;
        }
        Err(_) => {
            fail(502, Some("no-response"));
            reply(client, RESP_502).await;
            return None;
        }
    };

    if rhead.status == 101 {
        // The head and whatever the origin already sent behind it.
        if relay::write_flush(client, resp_buf).await.is_err() {
            return None;
        }
        if !early.is_empty() && !matches!(timeout(idle, up.write_all(&early)).await, Ok(Ok(()))) {
            return None;
        }
        return Some(Tunnel {
            up,
            kind: "websocket",
            status: 101,
            target: svc.log.all().then_some(key),
            t0: std::time::Instant::now(),
        });
    }

    // The origin refused the upgrade (or answered something else): relay its
    // answer as a plain response, then close.
    if rhead.status < 200 {
        fail(502, Some("unexpected-1xx"));
        reply(client, RESP_502).await;
        return None;
    }
    let budget = cfg.http.max_response_body_bytes;
    if budget > 0 && rhead.content_length.is_some_and(|l| l > budget) {
        fail(502, Some("over-budget"));
        reply(client, RESP_502).await;
        return None;
    }
    fail(rhead.status, Some("upgrade-refused"));
    if rhead.chunked {
        if relay::write_flush(client, &resp_buf[..rhead.raw_len])
            .await
            .is_ok()
        {
            let _ = relay::chunked(&mut up, client, resp_buf, rhead.raw_len, budget, idle).await;
        }
        return None;
    }
    let spare = (resp_buf.len() - rhead.raw_len) as u64;
    let first = rhead.content_length.map_or(spare, |l| l.min(spare)) as usize;
    if relay::write_flush(client, &resp_buf[..rhead.raw_len + first])
        .await
        .is_ok()
    {
        let left = rhead.content_length.map(|l| l - first as u64);
        relay::response(&mut up, client, left, idle).await;
    }
    None
}
