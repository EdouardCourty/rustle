//! Transparent chunked-body relay: forwards every byte verbatim (framing
//! included) while tracking boundaries, and returns once the terminal
//! chunk has been consumed — the upstream then stands at a clean response
//! boundary (candidate for pooling). Verbatim forwarding keeps
//! transfer-level details (chunk sizes, extensions, trailers) untouched.
//!
//! Parsing runs over a staging buffer (the connection's `resp_buf`
//! scratch, no allocation): one read per refill instead of one per byte,
//! and every parsed byte is forwarded in one write per refill. The bytes
//! already read along with the response head are the first staged bytes —
//! the parser starts exactly at the body. Large chunk data bypasses the
//! staging (adaptive relay straight from the socket).

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::adaptive::copy_limited;
use super::IdleTimer;

/// Minimum staging capacity (refill size).
const STAGE: usize = 4096;
/// Size line cap (hex + extensions + CRLF).
const MAX_SIZE_LINE: usize = 128;
/// Trailer caps: per line, and number of lines (an upstream streaming
/// endless short trailer lines must not pin the task).
const MAX_TRAILER_LINE: usize = 1024;
const MAX_TRAILER_LINES: usize = 64;

/// End of a chunked body.
pub struct ChunkedEnd {
    /// Chunk DATA bytes relayed (framing excluded).
    pub body: u64,
    /// Bytes were read past the terminal chunk: the upstream stream is
    /// desynchronized, never pool it.
    pub over_read: bool,
}

fn invalid(msg: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

/// Staging over `up`: `buf[pos..]` is unparsed, `buf[flushed..pos]` is
/// parsed but not yet forwarded to the client.
struct Staged<'a, R> {
    up: &'a mut R,
    client: &'a mut TcpStream,
    buf: &'a mut Vec<u8>,
    pos: usize,
    flushed: usize,
    timer: IdleTimer,
    idle: std::time::Duration,
}

impl<R: AsyncRead + Unpin> Staged<'_, R> {
    /// Forwards the parsed-but-pending bytes.
    async fn flush(&mut self) -> std::io::Result<()> {
        if self.pos > self.flushed {
            match self
                .timer
                .park(self.client.write_all(&self.buf[self.flushed..self.pos]))
                .await
            {
                Some(Ok(_)) => self.timer.reset(),
                _ => return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
            }
            self.flushed = self.pos;
        }
        Ok(())
    }

    /// Flushes, drops everything before `keep` (a partial line, small by
    /// the caps), then reads more. Returns the shift applied to indices.
    async fn refill(&mut self, keep: usize) -> std::io::Result<usize> {
        self.flush().await?;
        self.buf.drain(..keep);
        self.pos -= keep;
        self.flushed = self.pos;
        if self.buf.capacity() - self.buf.len() < STAGE / 2 {
            self.buf.reserve(STAGE);
        }
        match self.timer.park(self.up.read_buf(self.buf)).await {
            Some(Ok(0)) => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "chunked body truncated",
            )),
            Some(Ok(_)) => {
                self.timer.reset();
                Ok(keep)
            }
            Some(Err(e)) => Err(e),
            None => Err(std::io::Error::from(std::io::ErrorKind::TimedOut)),
        }
    }

    /// Consumes one line (through `\n`); returns its range in `buf`.
    async fn line(&mut self, cap: usize, too_long: &'static str) -> std::io::Result<(usize, usize)> {
        let mut start = self.pos;
        let mut scan = self.pos;
        loop {
            if let Some(i) = self.buf[scan..].iter().position(|&b| b == b'\n') {
                self.pos = scan + i + 1;
                if self.pos - start > cap {
                    return Err(invalid(too_long));
                }
                return Ok((start, self.pos));
            }
            if self.buf.len() - start > cap {
                return Err(invalid(too_long));
            }
            scan = self.buf.len();
            let shift = self.refill(start).await?;
            start -= shift;
            scan -= shift;
        }
    }

    /// Consumes exactly `n` bytes; returns their start in `buf`.
    async fn exact(&mut self, n: usize) -> std::io::Result<usize> {
        while self.buf.len() - self.pos < n {
            self.refill(self.pos).await?;
        }
        self.pos += n;
        Ok(self.pos - n)
    }
}

/// Strict chunk-size parse: 1..=16 hex digits, then `;ext`, `\r` or `\n`.
fn chunk_size(line: &[u8]) -> Option<u64> {
    let end = line
        .iter()
        .position(|&b| b == b';' || b == b'\r' || b == b'\n' || b == b' ' || b == b'\t')?;
    let hex = &line[..end];
    if hex.is_empty() || hex.len() > 16 {
        return None;
    }
    let mut v: u64 = 0;
    for &b in hex {
        let d = (b as char).to_digit(16)?;
        v = (v << 4) | d as u64;
    }
    Some(v)
}

/// Relays a chunked body. `stage[start..]` holds body bytes already read
/// (along with the response head); `stage` is reused as staging. `budget`
/// caps the DATA bytes (0 = unlimited): exceeding it is an error (caller
/// closes — a truncated stream is the honest signal).
pub async fn chunked<R: AsyncRead + Unpin>(
    up: &mut R,
    client: &mut TcpStream,
    stage: &mut Vec<u8>,
    start: usize,
    budget: u64,
    idle: std::time::Duration,
) -> std::io::Result<ChunkedEnd> {
    let mut s = Staged {
        up,
        client,
        buf: stage,
        pos: start,
        flushed: start,
        timer: IdleTimer::new(idle),
        idle,
    };
    let mut body: u64 = 0;
    loop {
        let (a, b) = s.line(MAX_SIZE_LINE, "chunk size line too long").await?;
        let size = chunk_size(&s.buf[a..b]).ok_or_else(|| invalid("bad chunk size"))?;

        if size == 0 {
            // Terminal chunk: trailer lines, ended by a bare CRLF.
            let mut lines = 0;
            loop {
                let (a, b) = s.line(MAX_TRAILER_LINE, "trailer line too long").await?;
                if &s.buf[a..b] == b"\r\n" {
                    break;
                }
                lines += 1;
                if lines > MAX_TRAILER_LINES {
                    return Err(invalid("too many trailer lines"));
                }
            }
            s.flush().await?;
            return Ok(ChunkedEnd {
                body,
                over_read: s.pos < s.buf.len(),
            });
        }

        body = body.saturating_add(size);
        if budget > 0 && body > budget {
            return Err(invalid("chunked body over budget"));
        }

        // Chunk data: staged bytes first (forwarded with the framing),
        // then the rest straight from the socket.
        let staged = ((s.buf.len() - s.pos) as u64).min(size);
        s.pos += staged as usize;
        let mut left = size - staged;
        if left > 0 {
            s.flush().await?;
            s.buf.clear();
            s.pos = 0;
            s.flushed = 0;
            while left > 0 {
                let n = copy_limited(s.up, s.client, Some(left), s.idle).await;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "chunk truncated",
                    ));
                }
                left -= n;
            }
            s.timer.reset();
        }

        let c = s.exact(2).await?;
        if &s.buf[c..c + 2] != b"\r\n" {
            return Err(invalid("bad chunk terminator"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;

    /// Runs the relay over `wire` (delivered as `prefix` + socket reads of
    /// at most `seg` bytes); returns (result, bytes the client received).
    async fn run(
        prefix: &[u8],
        wire: &[u8],
        seg: usize,
        budget: u64,
    ) -> (std::io::Result<ChunkedEnd>, Vec<u8>) {
        let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap();
        let reader = tokio::spawn(async move {
            let (mut peer, _) = ln.accept().await.unwrap();
            let mut got = Vec::new();
            peer.read_to_end(&mut got).await.unwrap();
            got
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let (mut up_w, mut up_r) = tokio::io::duplex(64);
        let wire = wire.to_vec();
        tokio::spawn(async move {
            for c in wire.chunks(seg) {
                if up_w.write_all(c).await.is_err() {
                    return;
                }
            }
        });
        let mut stage = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        let start = stage.len();
        stage.extend_from_slice(prefix);
        let res = chunked(&mut up_r, &mut client, &mut stage, start, budget, Duration::from_secs(2)).await;
        drop(client);
        (res, reader.await.unwrap())
    }

    const SMALL: &[u8] = b"5\r\nhello\r\n0\r\n\r\n";

    #[tokio::test]
    async fn whole_body_in_prefix() {
        let (r, got) = run(SMALL, b"", 1, 0).await;
        let end = r.unwrap();
        assert_eq!((end.body, end.over_read), (5, false));
        assert_eq!(got, SMALL);
    }

    #[tokio::test]
    async fn coalesced_and_byte_by_byte() {
        for seg in [1, 3, 7, 1024] {
            let (r, got) = run(b"", SMALL, seg, 0).await;
            assert_eq!(r.unwrap().body, 5, "seg={seg}");
            assert_eq!(got, SMALL, "seg={seg}");
        }
    }

    #[tokio::test]
    async fn prefix_split_mid_chunk() {
        let (r, got) = run(&SMALL[..6], &SMALL[6..], 2, 0).await;
        assert_eq!(r.unwrap().body, 5);
        assert_eq!(got, SMALL);
    }

    #[tokio::test]
    async fn large_chunk_extensions_trailers() {
        let data = vec![b'x'; 100_000];
        let mut wire = format!("{:x};ext=1\r\n", data.len()).into_bytes();
        wire.extend_from_slice(&data);
        wire.extend_from_slice(b"\r\n3\r\nabc\r\n0\r\nX-T: 1\r\n\r\n");
        let (r, got) = run(b"", &wire, 8192, 0).await;
        assert_eq!(r.unwrap().body, 100_003);
        assert_eq!(got, wire);
    }

    #[tokio::test]
    async fn over_read_detected() {
        let mut wire = SMALL.to_vec();
        wire.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
        let (r, got) = run(&wire, b"", 1, 0).await;
        assert!(r.unwrap().over_read);
        assert_eq!(got, SMALL);
    }

    #[tokio::test]
    async fn rejects_bad_framing() {
        for bad in [
            &b"+5\r\nhello\r\n0\r\n\r\n"[..],
            b"zz\r\n",
            b"5\r\nhelloXX0\r\n\r\n",
            b"5\r\nhel",
        ] {
            assert!(run(bad, b"", 1, 0).await.0.is_err(), "{:?}", bad);
        }
    }

    #[tokio::test]
    async fn budget_and_trailer_caps() {
        assert!(run(SMALL, b"", 1, 4).await.0.is_err());
        let mut wire = b"0\r\n".to_vec();
        for _ in 0..=MAX_TRAILER_LINES {
            wire.extend_from_slice(b"a: b\r\n");
        }
        wire.extend_from_slice(b"\r\n");
        assert!(run(&wire, b"", 1, 0).await.0.is_err());
    }
}
