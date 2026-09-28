//! Transparent chunked-body relay: forwards every byte verbatim (framing
//! included) while tracking boundaries, and returns once the terminal
//! chunk has been consumed — the upstream then stands at a clean response
//! boundary (candidate for pooling). Verbatim forwarding keeps
//! transfer-level details (chunk sizes, extensions, trailers) untouched.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::adaptive::copy_limited;

pub async fn chunked(up: &mut TcpStream, client: &mut TcpStream) -> std::io::Result<u64> {
    let byte = &mut [0u8; 1];
    let mut total: u64 = 0;
    loop {
        // ---- size line: hex [;ext] CRLF, read byte-wise, forward verbatim
        let mut size_line: Vec<u8> = Vec::with_capacity(16);
        loop {
            up.read_exact(byte).await?;
            let b = byte[0];
            size_line.push(b);
            if b == b'\n' {
                break;
            }
            if size_line.len() > 128 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "chunk size line too long",
                ));
            }
        }
        let hex_end = size_line
            .iter()
            .position(|&b| b == b';' || b == b'\r' || b == b'\n')
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no chunk size"))?;
        let chunk_size = std::str::from_utf8(&size_line[..hex_end])
            .ok()
            .and_then(|s| u64::from_str_radix(s, 16).ok())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size")
            })?;

        client.write_all(&size_line).await?;

        if chunk_size == 0 {
            // Terminal chunk: zero or more trailer lines, ended by a bare
            // CRLF; all forwarded verbatim, then the upstream stands at a
            // clean response boundary.
            loop {
                let mut trailer: Vec<u8> = Vec::with_capacity(64);
                loop {
                    up.read_exact(byte).await?;
                    let b = byte[0];
                    trailer.push(b);
                    if b == b'\n' {
                        break;
                    }
                }
                client.write_all(&trailer).await?;
                if trailer == b"\r\n" {
                    return Ok(total);
                }
                if trailer.len() > 1024 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "trailer line too long",
                    ));
                }
            }
        }

        // ---- chunk data (transparent, adaptively relayed), then its CRLF
        let mut left = chunk_size;
        while left > 0 {
            let n = copy_limited(up, client, Some(left)).await;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "chunk truncated",
                ));
            }
            left -= n;
            total += n;
        }
        let mut crlf = [0u8; 2];
        up.read_exact(&mut crlf).await?;
        client.write_all(&crlf).await?;
        if crlf != [b'\r', b'\n'] {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bad chunk terminator",
            ));
        }
    }
}
