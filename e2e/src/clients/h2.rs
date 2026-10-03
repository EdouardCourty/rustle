//! HTTP/2 client (ALPN `h2` on the proxy's TLS port). CONNECT streams are
//! exposed as an AsyncRead + AsyncWrite tunnel.

use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{ready, Context as Cx, Poll};

use anyhow::Context;
use bytes::Bytes;
use h2::client::SendRequest;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::harness::certs;

pub async fn connect(addr: SocketAddr) -> anyhow::Result<SendRequest<Bytes>> {
    let tls = super::tls_connect(addr, certs::client(&[b"h2", b"http/1.1"])).await?;
    anyhow::ensure!(
        tls.get_ref().1.alpn_protocol() == Some(b"h2"),
        "ALPN did not select h2"
    );
    let (send, conn) = h2::client::Builder::new()
        .initial_window_size(1 << 20)
        .initial_connection_window_size(4 << 20)
        .handshake(tls)
        .await
        .context("h2 handshake")?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(send)
}

pub struct H2Response {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

/// One request. `body`: (bytes, declare Content-Length).
pub async fn request(
    send: &mut SendRequest<Bytes>,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<(&[u8], bool)>,
) -> anyhow::Result<H2Response> {
    let mut b = http::Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    if let Some((data, true)) = body {
        b = b.header("content-length", data.len());
    }
    let req = b.body(())?;
    let mut send = send.clone().ready().await.context("h2 not ready")?;
    let (resp, stream) = send.send_request(req, body.is_none())?;
    if let Some((data, _)) = body {
        let mut tx = H2Write { s: stream };
        tokio::io::AsyncWriteExt::write_all(&mut tx, data).await?;
        tokio::io::AsyncWriteExt::shutdown(&mut tx).await?;
    }
    let resp = resp.await.context("h2 response")?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let mut recv = resp.into_body();
    let mut out = Vec::new();
    while let Some(chunk) = recv.data().await {
        let chunk = chunk.context("h2 body")?;
        let _ = recv.flow_control().release_capacity(chunk.len());
        out.extend_from_slice(&chunk);
    }
    recv.trailers().await.context("h2 trailers")?;
    Ok(H2Response {
        status,
        headers,
        body: out,
    })
}

/// CONNECT `authority` as a stream; Ok((status, tunnel when 200)).
pub async fn connect_tunnel(
    send: &mut SendRequest<Bytes>,
    authority: &str,
    auth: Option<&str>,
) -> anyhow::Result<(u16, Option<Tunnel>)> {
    let mut b = http::Request::builder().method("CONNECT").uri(authority);
    if let Some(a) = auth {
        b = b.header("proxy-authorization", a);
    }
    let mut send = send.clone().ready().await?;
    let (resp, stream) = send.send_request(b.body(())?, false)?;
    let resp = resp.await.context("h2 CONNECT response")?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Ok((status, None));
    }
    Ok((
        status,
        Some(Tunnel {
            w: H2Write { s: stream },
            r: H2Read {
                s: resp.into_body(),
                pending: Bytes::new(),
            },
        }),
    ))
}

pub struct H2Write {
    s: h2::SendStream<Bytes>,
}

pub struct H2Read {
    s: h2::RecvStream,
    pending: Bytes,
}

pub struct Tunnel {
    w: H2Write,
    r: H2Read,
}

fn io_err(e: h2::Error) -> std::io::Error {
    std::io::Error::other(e)
}

impl AsyncWrite for H2Write {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Cx<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        self.s.reserve_capacity(buf.len().min(1 << 20));
        match ready!(self.s.poll_capacity(cx)) {
            Some(Ok(n)) if n > 0 => {
                let n = n.min(buf.len());
                self.s
                    .send_data(Bytes::copy_from_slice(&buf[..n]), false)
                    .map_err(io_err)?;
                Poll::Ready(Ok(n))
            }
            Some(Ok(_)) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Some(Err(e)) => Poll::Ready(Err(io_err(e))),
            None => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Cx<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _: &mut Cx<'_>) -> Poll<std::io::Result<()>> {
        self.s.reserve_capacity(0);
        Poll::Ready(self.s.send_data(Bytes::new(), true).map_err(io_err))
    }
}

impl AsyncRead for H2Read {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Cx<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pending.is_empty() {
            match ready!(self.s.poll_data(cx)) {
                Some(Ok(d)) => {
                    let _ = self.s.flow_control().release_capacity(d.len());
                    self.pending = d;
                }
                Some(Err(e)) => return Poll::Ready(Err(io_err(e))),
                None => return Poll::Ready(Ok(())),
            }
        }
        let n = self.pending.len().min(buf.remaining());
        buf.put_slice(&self.pending.split_to(n));
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for Tunnel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Cx<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.r).poll_read(cx, buf)
    }
}

impl AsyncWrite for Tunnel {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Cx<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.w).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Cx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.w).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Cx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.w).poll_shutdown(cx)
    }
}
