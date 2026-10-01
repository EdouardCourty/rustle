//! Kernel TLS (kTLS) for the HTTPS proxy listener.
//!
//! rustls performs the handshake; the session keys are then handed to the
//! kernel (`TCP_ULP "tls"` + `SOL_TLS` TX/RX) and the socket becomes a plain
//! socket again from userspace: writes are encrypted, reads decrypted by
//! the kernel. The zero-copy paths come back on the TLS listener — splice
//! for tunnels and bulk responses — and records are no longer encrypted
//! through a userspace copy.
//!
//! - Availability is probed once at startup (the `tls` module must be
//!   loaded on the host): extracting the secrets consumes the rustls
//!   session, there is no way back to userspace TLS for that connection,
//!   so a failing setup must never be discovered per connection.
//! - Bytes rustls already decrypted past the handshake ("drained") are
//!   served first; zero-copy is only offered once they are consumed.
//! - A non-data record at the head of the receive queue (alert,
//!   close_notify, post-handshake message) makes `read` fail with EIO:
//!   treated as the end of the client stream.
//! - Shutdown sends a close_notify alert record, then FIN.

use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use ktls::{CorkStream, CryptoInfo};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

use crate::stream::ClientStream;

/// TLS session before offload: rustls over a corkable socket (the cork
/// stops reads at a record boundary so rustls can be drained cleanly).
pub type CorkedTls = tokio_rustls::server::TlsStream<CorkStream<TcpStream>>;

const SOL_TCP: libc::c_int = 6;
const TCP_ULP: libc::c_int = 31;
const SOL_TLS: libc::c_int = 282;
const TLS_TX: libc::c_int = 1;
const TLS_RX: libc::c_int = 2;
const TLS_SET_RECORD_TYPE: libc::c_int = 1;
/// Kernel ≥ 6.0: encrypt splice/sendfile pages in place (no copy of the
/// plaintext); safe here, spliced pages come from a pipe nobody rewrites.
const TLS_TX_ZEROCOPY_RO: libc::c_int = 3;
/// Kernel ≥ 6.0, TLS 1.3: decrypt straight into the reader's buffer.
const TLS_RX_EXPECT_NO_PAD: libc::c_int = 4;
const RECORD_ALERT: u8 = 21;

fn os_err() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn other<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

fn setup_ulp(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: plain setsockopt with a 3-byte string.
    let r = unsafe { libc::setsockopt(fd, SOL_TCP, TCP_ULP, b"tls".as_ptr().cast(), 3) };
    if r < 0 {
        return Err(os_err());
    }
    Ok(())
}

fn setup_keys(fd: RawFd, dir: libc::c_int, info: &CryptoInfo) -> std::io::Result<()> {
    // SAFETY: `info` is the kernel's tls12_crypto_info_* struct, size given.
    let r = unsafe { libc::setsockopt(fd, SOL_TLS, dir, info.as_ptr(), info.size() as libc::socklen_t) };
    if r < 0 {
        return Err(os_err());
    }
    Ok(())
}

/// True when the kernel accepts the TLS ULP (module `tls` loaded): tried
/// on a loopback connection, the ULP needs an established socket.
pub fn available() -> bool {
    let probe = || -> std::io::Result<()> {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let _c = std::net::TcpStream::connect(l.local_addr()?)?;
        let (s, _) = l.accept()?;
        setup_ulp(s.as_raw_fd())
    };
    probe().is_ok()
}

/// Hands a completed rustls session to the kernel.
pub async fn offload(mut tls: CorkedTls) -> std::io::Result<KtlsClient> {
    // Pending handshake records (session tickets) go out under rustls.
    tls.flush().await?;
    tls.get_mut().0.corked = true;
    // Application bytes rustls already decrypted, up to a record boundary.
    let mut drained = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match tls.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => drained.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
    }
    let (cork, conn) = tls.into_inner();
    let tcp = cork.io;
    let suite = conn.negotiated_cipher_suite().ok_or_else(|| other("no cipher suite"))?;
    let secrets = conn.dangerous_extract_secrets().map_err(other)?;
    let fd = tcp.as_raw_fd();
    setup_ulp(fd)?;
    setup_keys(fd, TLS_TX, &CryptoInfo::from_rustls(suite, secrets.tx).map_err(other)?)?;
    setup_keys(fd, TLS_RX, &CryptoInfo::from_rustls(suite, secrets.rx).map_err(other)?)?;
    // Best effort (older kernels refuse them; TLS 1.2 ignores NO_PAD).
    let one: libc::c_int = 1;
    for opt in [TLS_TX_ZEROCOPY_RO, TLS_RX_EXPECT_NO_PAD] {
        // SAFETY: int option, size given.
        unsafe {
            libc::setsockopt(fd, SOL_TLS, opt, (&one as *const libc::c_int).cast(), 4);
        }
    }
    Ok(KtlsClient {
        tcp,
        drained,
        pos: 0,
        closed: false,
    })
}

/// A client connection whose TLS records are handled by the kernel.
pub struct KtlsClient {
    tcp: TcpStream,
    drained: Vec<u8>,
    pos: usize,
    closed: bool,
}

impl KtlsClient {
    /// No decrypted byte left in userspace: the socket can be used as-is.
    #[inline]
    fn raw_ready(&self) -> bool {
        self.pos >= self.drained.len()
    }
}

/// close_notify: an alert record (level warning, description 0) sent
/// through the kernel with the record type in a control message.
fn send_close_notify(fd: RawFd) -> std::io::Result<()> {
    let mut payload = [1u8, 0u8];
    // u64 storage keeps the control buffer aligned for cmsghdr.
    let mut cbuf = [0u64; 8];
    // SAFETY: msghdr/cmsghdr built per cmsg(3) in a buffer large enough
    // for CMSG_SPACE(1); all pointers outlive the sendmsg call.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: payload.as_mut_ptr().cast(),
            iov_len: payload.len(),
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(1) as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = SOL_TLS;
        (*c).cmsg_type = TLS_SET_RECORD_TYPE;
        (*c).cmsg_len = libc::CMSG_LEN(1) as _;
        *libc::CMSG_DATA(c) = RECORD_ALERT;
        if libc::sendmsg(fd, &msg, 0) < 0 {
            return Err(os_err());
        }
    }
    Ok(())
}

impl AsyncRead for KtlsClient {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.raw_ready() {
            let k = (self.drained.len() - self.pos).min(buf.remaining());
            buf.put_slice(&self.drained[self.pos..self.pos + k]);
            self.pos += k;
            if self.raw_ready() {
                self.drained = Vec::new();
                self.pos = 0;
            }
            return Poll::Ready(Ok(()));
        }
        match Pin::new(&mut self.tcp).poll_read(cx, buf) {
            // Non-data record (alert / close_notify / post-handshake
            // message) at the head of the queue: end of the client stream.
            Poll::Ready(Err(e)) if e.raw_os_error() == Some(libc::EIO) => Poll::Ready(Ok(())),
            other => other,
        }
    }
}

impl AsyncWrite for KtlsClient {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.tcp).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.tcp).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.tcp).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if !self.closed {
            self.closed = true;
            let _ = send_close_notify(self.tcp.as_raw_fd());
        }
        Pin::new(&mut self.tcp).poll_shutdown(cx)
    }
}

impl ClientStream for KtlsClient {
    /// Still a TLS session: close_notify on close.
    const TLS: bool = true;
    const VIA: &'static str = "ktls";

    /// The socket itself carries plaintext for userspace once the drained
    /// bytes are consumed: splice and socket options apply.
    #[inline]
    fn tcp(&mut self) -> Option<&mut TcpStream> {
        if self.raw_ready() {
            Some(&mut self.tcp)
        } else {
            None
        }
    }

    #[inline]
    fn into_tcp(self) -> Result<TcpStream, Self> {
        if self.raw_ready() {
            Ok(self.tcp)
        } else {
            Err(self)
        }
    }
}
