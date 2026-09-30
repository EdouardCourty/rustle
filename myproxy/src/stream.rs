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

pub type TlsClient = tokio_rustls::server::TlsStream<TcpStream>;

pub trait ClientStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    /// TLS session: needs close_notify on close, no zero-copy.
    const TLS: bool;

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

    #[inline]
    fn tcp(&mut self) -> Option<&mut TcpStream> {
        None
    }

    #[inline]
    fn into_tcp(self) -> Result<TcpStream, Self> {
        Err(self)
    }
}
