//! Client-side transport: plain TCP or TLS.
//!
//! The pipeline is generic over [`ClientStream`] and monomorphized per
//! transport: on plain TCP every `tcp()` / `into_tcp()` is statically
//! `Some`/`Ok`, so the fast paths (splice, TCP_QUICKACK) compile exactly as
//! before. On TLS the socket carries records, not application bytes: those
//! paths fall back to user-space copies.
//!
//! Writes on TLS may leave ciphertext buffered inside the session after
//! `write_all` returns: every place that ends a write burst also calls
//! `flush()` (a no-op on TCP).

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

pub type TlsClient = tokio_rustls::server::TlsStream<ReadAhead>;

/// Read-ahead under rustls. rustls reads the socket 4 KB at a time
/// (`READ_SIZE` in its deframer): ~4 `recv` per 16 KB record, the dominant
/// cost of a TLS bulk flow. Adaptive, like the relay buffers: direct reads
/// while the flow is light (no memory held by idle connections); a read
/// that fills the whole request switches to a 64 KB buffer, dropped again
/// as soon as a refill comes back short.
pub struct ReadAhead {
    tcp: TcpStream,
    buf: Vec<u8>,
    pos: usize,
}

const READ_AHEAD: usize = 64 * 1024;

impl ReadAhead {
    pub fn new(tcp: TcpStream) -> Self {
        Self {
            tcp,
            buf: Vec::new(),
            pos: 0,
        }
    }
}

impl AsyncRead for ReadAhead {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;
        let this = &mut *self;
        if this.pos < this.buf.len() {
            let k = (this.buf.len() - this.pos).min(out.remaining());
            out.put_slice(&this.buf[this.pos..this.pos + k]);
            this.pos += k;
            return Poll::Ready(Ok(()));
        }
        if this.buf.capacity() == 0 {
            // Light mode: straight into the caller's buffer.
            let want = out.remaining();
            let before = out.filled().len();
            let r = std::pin::Pin::new(&mut this.tcp).poll_read(cx, out);
            if let Poll::Ready(Ok(())) = r {
                if want > 0 && out.filled().len() - before == want {
                    this.buf = Vec::with_capacity(READ_AHEAD); // bulk: read ahead
                }
            }
            return r;
        }
        // Bulk mode: refill the read-ahead buffer.
        this.buf.clear();
        this.pos = 0;
        let mut rb = tokio::io::ReadBuf::uninit(this.buf.spare_capacity_mut());
        match std::pin::Pin::new(&mut this.tcp).poll_read(cx, &mut rb) {
            Poll::Ready(Ok(())) => {
                let n = rb.filled().len();
                // SAFETY: `n` bytes of the spare capacity were initialized
                // by the read.
                unsafe { this.buf.set_len(n) };
                let k = n.min(out.remaining());
                out.put_slice(&this.buf[..k]);
                this.pos = k;
                if n < READ_AHEAD / 4 && this.pos == this.buf.len() {
                    this.buf = Vec::new(); // the flow went light: release
                    this.pos = 0;
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl AsyncWrite for ReadAhead {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        b: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.tcp).poll_write(cx, b)
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.tcp).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.tcp).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.tcp).poll_shutdown(cx)
    }
}

pub trait ClientStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    /// TLS session: needs close_notify on close, no zero-copy.
    const TLS: bool;
    /// Transport name in the access log.
    const VIA: &'static str;

    /// The TCP socket when it carries the application bytes as-is (plain
    /// TCP): enables splice and socket options.
    fn tcp(&mut self) -> Option<&mut TcpStream>;

    /// Owned TCP socket for the zero-copy tunnel (plain TCP only).
    fn into_tcp(self) -> Result<TcpStream, Self>
    where
        Self: Sized;
}

impl ClientStream for TcpStream {
    const TLS: bool = false;
    const VIA: &'static str = "http1";

    #[inline]
    fn tcp(&mut self) -> Option<&mut TcpStream> {
        Some(self)
    }

    #[inline]
    fn into_tcp(self) -> Result<TcpStream, Self> {
        Ok(self)
    }
}

impl ClientStream for TlsClient {
    const TLS: bool = true;
    const VIA: &'static str = "tls";

    #[inline]
    fn tcp(&mut self) -> Option<&mut TcpStream> {
        None
    }

    #[inline]
    fn into_tcp(self) -> Result<TcpStream, Self> {
        Err(self)
    }
}
