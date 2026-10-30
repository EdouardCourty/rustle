//! Origin server of the benchmark: static objects (`/1kb`, `/10kb`, `/1mb`,
//! `/10mb`) served from memory, HTTP/1.1 keep-alive. Replaces nginx, whose
//! per-request cost (~12 µs, twice the generator's) was the harness ceiling.
//! Responses (head + body) are built once and written with one `write_all`;
//! `Connection: close` requests get their own prebuilt variant.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::rt::{bind_listener, run_threads};

struct Object {
    path: &'static str,
    keep_alive: Vec<u8>,
    close: Vec<u8>,
}

fn build(size: usize, path: &'static str) -> Object {
    let mk = |conn: &str| {
        let mut v = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {size}\r\nConnection: {conn}\r\n\r\n"
        )
        .into_bytes();
        v.resize(v.len() + size, 0);
        v
    };
    Object {
        path,
        keep_alive: mk("keep-alive"),
        close: mk("close"),
    }
}

/// WebSocket handshake answer. The accept value matches the fixed
/// `Sec-WebSocket-Key` of the generator (RFC 6455 example), so no SHA-1 is
/// needed; the proxy under test never looks at it.
const WS_101: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
const WS_426: &[u8] = b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\n\r\n";

/// After the 101: echo every byte back verbatim (frames are symmetric in
/// length, so the generator reads back exactly what it sent) without
/// parsing them: the cheapest origin, never the bottleneck.
async fn ws_echo(mut s: TcpStream, pending: Vec<u8>) {
    if !pending.is_empty() && s.write_all(&pending).await.is_err() {
        return;
    }
    let mut b = vec![0u8; 256 * 1024];
    loop {
        match s.read(&mut b).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if s.write_all(&b[..n]).await.is_err() {
                    return;
                }
            }
        }
    }
}

const NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
const BAD: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

async fn serve(mut s: TcpStream, objects: &'static [Object]) {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    loop {
        // Serve every complete request already buffered (pipelining).
        loop {
            let mut headers = [httparse::EMPTY_HEADER; 32];
            let mut req = httparse::Request::new(&mut headers);
            let (len, close, path, ws) = match req.parse(&buf) {
                Ok(httparse::Status::Complete(len)) => {
                    let close = req.version == Some(0)
                        || req.headers.iter().any(|h| {
                            h.name.eq_ignore_ascii_case("connection")
                                && h.value.windows(5).any(|w| w.eq_ignore_ascii_case(b"close"))
                        });
                    let ws = req.headers.iter().any(|h| {
                        h.name.eq_ignore_ascii_case("upgrade")
                            && h.value.eq_ignore_ascii_case(b"websocket")
                    });
                    (len, close, req.path.unwrap_or("/"), ws)
                }
                Ok(httparse::Status::Partial) => break,
                Err(_) => {
                    let _ = s.write_all(BAD).await;
                    return;
                }
            };
            if path == "/ws" {
                if !ws {
                    if s.write_all(WS_426).await.is_err() {
                        return;
                    }
                    buf.drain(..len);
                    continue;
                }
                if s.write_all(WS_101).await.is_err() {
                    return;
                }
                let pending = buf[len..].to_vec();
                ws_echo(s, pending).await;
                return;
            }
            let obj = objects
                .iter()
                .find(|o| path.strip_prefix('/') == Some(o.path));
            let out: &[u8] = match obj {
                Some(o) if close => &o.close,
                Some(o) => &o.keep_alive,
                None => NOT_FOUND,
            };
            if s.write_all(out).await.is_err() || close {
                return;
            }
            buf.drain(..len);
        }
        match s.read_buf(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if buf.len() > 65536 {
            return;
        }
    }
}

pub(crate) fn run_upstream(listen: &str, threads: usize) -> ! {
    let objects: &'static [Object] = Box::leak(
        vec![
            build(1024, "1kb"),
            build(10 * 1024, "10kb"),
            build(1024 * 1024, "1mb"),
            build(10 * 1024 * 1024, "10mb"),
        ]
        .into_boxed_slice(),
    );
    let listen = listen.to_string();
    let first = match bind_listener(&listen, threads > 1) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("upstream: bind {listen} failed: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("upstream: listening on {listen} ({threads} threads)");
    let first = std::sync::Mutex::new(Some(first));
    run_threads(threads, move |i| {
        let l = if i == 0 {
            first.lock().unwrap().take()
        } else {
            None
        };
        let listen = listen.clone();
        async move {
            let std_l = match l {
                Some(l) => l,
                None => bind_listener(&listen, true).expect("upstream: bind"),
            };
            let listener = TcpListener::from_std(std_l).expect("upstream: listener");
            loop {
                match listener.accept().await {
                    Ok((s, _)) => {
                        let _ = s.set_nodelay(true);
                        tokio::spawn(serve(s, objects));
                    }
                    Err(e) => eprintln!("upstream: accept error: {e}"),
                }
            }
        }
    });
    unreachable!()
}
