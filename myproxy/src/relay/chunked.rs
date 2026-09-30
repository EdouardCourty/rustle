//! Transparent chunked-body relay: forwards every byte verbatim (framing
//! included) while tracking boundaries, and returns once the terminal
//! chunk has been consumed — the upstream then stands at a clean response
//! boundary (candidate for pooling). Verbatim forwarding keeps
//! transfer-level details (chunk sizes, extensions, trailers) untouched.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::adaptive::copy_limited;
use super::IdleTimer;

pub async fn chunked(
    up: &mut TcpStream,
    client: &mut TcpStream,
    idle: std::time::Duration,
) -> std::io::Result<u64> {
    let mut timer = IdleTimer::new(idle);
    let byte = &mut [0u8; 1];
    let mut total: u64 = 0;
    loop {
        // ---- size line: hex [;ext] CRLF, read byte-wise, forward verbatim
        let mut size_line: Vec<u8> = Vec::with_capacity(16);
        loop {
            match timer.park(up.read_exact(byte)).await {
                Some(Ok(_)) => {}
                None => {
                    return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
                }
                Some(Err(e)) => return Err(e),
            }
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

        match timer.park(client.write_all(&size_line)).await {
            Some(Ok(_)) => {}
            _ => return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        }

        if chunk_size == 0 {
            // Terminal chunk: zero or more trailer lines, ended by a bare
            // CRLF; all forwarded verbatim, then the upstream stands at a
            // clean response boundary. B3: the byte/line caps live INSIDE
            // the read loop — an upstream sending megabytes without \n
            // must not grow the Vec (DoS) or stall the task forever.
            loop {
                let mut trailer: Vec<u8> = Vec::with_capacity(64);
                loop {
                    match timer.park(up.read_exact(byte)).await {
                        Some(Ok(_)) => {}
                        None => {
                            return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
                        }
                        Some(Err(e)) => return Err(e),
                    }
                    let b = byte[0];
                    trailer.push(b);
                    if b == b'\n' {
                        break;
                    }
                    if trailer.len() > 1024 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "trailer line too long",
                        ));
                    }
                }
                match timer.park(client.write_all(&trailer)).await {
                    Some(Ok(_)) => {}
                    _ => return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
                }
                if trailer == b"\r\n" {
                    return Ok(total);
                }
            }
        }

        // ---- chunk data (transparent, adaptively relayed), then its CRLF
        let mut left = chunk_size;
        while left > 0 {
            let n = copy_limited(up, client, Some(left), idle).await;
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
        match timer.park(up.read_exact(&mut crlf)).await {
            Some(Ok(_)) => {}
            None => return Err(std::io::Error::from(std::io::ErrorKind::TimedOut)),
            Some(Err(e)) => return Err(e),
        }
        match timer.park(client.write_all(&crlf)).await {
            Some(Ok(_)) => {}
            _ => return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        }
        if crlf != *b"\r\n" {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bad chunk terminator",
            ));
        }
    }
}
