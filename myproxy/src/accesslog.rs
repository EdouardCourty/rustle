//! Access log: one line per event in a dedicated file, written by its own
//! thread — the proxy never blocks on it.
//!
//! - `off`: nothing (a branch per event site). `errors`: refusals and
//!   failures only (403 policy, wrong key, ban, 5xx, bad requests) — no
//!   cost in normal traffic. `all`: plus one line per HTTP request / h2
//!   stream and one per tunnel (CONNECT, SOCKS5, UDP) when it closes.
//! - Lines are formatted by the connection task into a per-shard (thread
//!   local, lock-free) batch, handed to the writer when it reaches 64 KB or
//!   by the shard's 200 ms flush tick: one writer wake-up per batch, not per
//!   line (a wake-up per line cost +25 % CPU/request at 1 KB, measured).
//!   The hand-off is a bounded channel with `try_send`: when the writer
//!   falls behind, batches are dropped and their lines counted
//!   (`event=dropped`), never waited for.
//! - Refusals at accept (client allowlist, banned IP) are only counted and
//!   summarized once per second: a scan cannot flood the file.
//! - SIGHUP reopens the file (logrotate: rename, then `kill -HUP`).
//!
//! Format: `<RFC 3339 UTC> key=value ...`, values sanitized (no spaces or
//! control characters) so a line is always one line.

use std::fmt::Write as _;
use std::io::Write as _;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use serde::Deserialize;

/// Which events are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Off,
    #[default]
    Errors,
    All,
}

/// How a plain-HTTP request target is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TargetMode {
    /// `host:port` (same detail as a tunnel).
    #[default]
    Host,
    /// Full URL (plain HTTP only; tunnels stay `host:port`).
    Url,
}

/// Batches waiting for the writer before new ones are dropped.
const QUEUE: usize = 1024;
/// Batch size that triggers a hand-off without waiting for the tick.
const BATCH: usize = 64 * 1024;

thread_local! {
    /// The current shard's pending lines.
    static PENDING: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}
/// Writer wake-up period (flush, summaries, reopen).
const TICK: Duration = Duration::from_millis(200);

/// Counters shared with the writer thread.
#[derive(Default)]
struct Counters {
    dropped: AtomicU64,
    refused_allowlist: AtomicU64,
    refused_banned: AtomicU64,
    last_refused: Mutex<Option<IpAddr>>,
    reopen: AtomicBool,
}

pub struct AccessLog {
    level: Level,
    target: TargetMode,
    tx: Option<SyncSender<Vec<u8>>>,
    counters: Arc<Counters>,
}

/// Why a connection was refused at accept.
#[derive(Clone, Copy)]
pub enum Refusal {
    Allowlist,
    Banned,
}

impl AccessLog {
    /// A log that writes nothing.
    pub fn disabled() -> Self {
        Self {
            level: Level::Off,
            target: TargetMode::Host,
            tx: None,
            counters: Arc::new(Counters::default()),
        }
    }

    /// Opens the file and starts the writer (disabled when `file` is empty
    /// or `level` is off).
    pub fn start(level: Level, file: &str, target: TargetMode) -> anyhow::Result<Self> {
        if file.is_empty() || level == Level::Off {
            return Ok(Self::disabled());
        }
        let path = file.to_string();
        let out = open(&path).with_context(|| format!("opening access log {path}"))?;
        let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE);
        let counters = Arc::new(Counters::default());
        let c = counters.clone();
        std::thread::Builder::new()
            .name("myproxy-accesslog".into())
            .spawn(move || writer(rx, out, path, c))?;
        Ok(Self {
            level,
            target,
            tx: Some(tx),
            counters,
        })
    }

    /// Per-request/tunnel lines wanted.
    #[inline]
    pub fn all(&self) -> bool {
        self.level == Level::All
    }

    /// Failure/refusal lines wanted.
    #[inline]
    pub fn errors(&self) -> bool {
        self.level >= Level::Errors
    }

    #[inline]
    pub fn target_mode(&self) -> TargetMode {
        self.target
    }

    /// Starts a line (timestamp written); finish it with [`Line`] setters
    /// and [`AccessLog::emit`].
    pub fn line(&self) -> Line {
        let mut b = Vec::with_capacity(160);
        rfc3339(&mut b, SystemTime::now());
        Line(b)
    }

    /// Appends a line to this shard's batch (handed to the writer when
    /// full, or by [`AccessLog::flush_local`]).
    pub fn emit(&self, line: Line) {
        if self.tx.is_none() {
            return;
        }
        let full = PENDING.with(|p| {
            let mut p = p.borrow_mut();
            p.extend_from_slice(&line.0);
            p.push(b'\n');
            p.len() >= BATCH
        });
        if full {
            self.flush_local();
        }
    }

    /// Hands this shard's batch to the writer (shard tick, shutdown).
    pub fn flush_local(&self) {
        let Some(tx) = &self.tx else { return };
        let batch = PENDING.with(|p| {
            let mut p = p.borrow_mut();
            if p.is_empty() {
                return None;
            }
            Some(std::mem::replace(&mut *p, Vec::with_capacity(BATCH)))
        });
        let Some(batch) = batch else { return };
        if let Err(TrySendError::Full(b)) = tx.try_send(batch) {
            let lines = b.iter().filter(|&&c| c == b'\n').count() as u64;
            self.counters.dropped.fetch_add(lines, Relaxed);
        }
    }

    /// Period of the per-shard flush tick.
    pub const TICK: Duration = TICK;

    /// Refusal at accept: counted, summarized by the writer.
    pub fn refused(&self, ip: IpAddr, why: Refusal) {
        if !self.errors() {
            return;
        }
        match why {
            Refusal::Allowlist => self.counters.refused_allowlist.fetch_add(1, Relaxed),
            Refusal::Banned => self.counters.refused_banned.fetch_add(1, Relaxed),
        };
        if let Ok(mut l) = self.counters.last_refused.lock() {
            *l = Some(ip);
        }
    }

    /// Asks the writer to reopen the file (SIGHUP, after logrotate).
    pub fn reopen(&self) {
        self.counters.reopen.store(true, Relaxed);
    }
}

/// One log line under construction.
pub struct Line(Vec<u8>);

impl Line {
    /// ` key=value`, the value sanitized (spaces and controls -> `_`).
    pub fn kv(mut self, key: &str, value: impl std::fmt::Display) -> Self {
        self.0.push(b' ');
        self.0.extend_from_slice(key.as_bytes());
        self.0.push(b'=');
        let start = self.0.len();
        let _ = write!(Buf(&mut self.0), "{value}");
        for b in &mut self.0[start..] {
            if *b <= b' ' || *b == 0x7f {
                *b = b'_';
            }
        }
        if self.0.len() == start {
            self.0.push(b'-');
        }
        self
    }

    /// ` key=<bytes>` for raw request bytes (lossy UTF-8, sanitized).
    pub fn kv_bytes(self, key: &str, value: &[u8]) -> Self {
        self.kv(key, String::from_utf8_lossy(value))
    }
}

/// `fmt::Write` over a byte vector.
struct Buf<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Buf<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

fn open(path: &str) -> std::io::Result<std::io::BufWriter<std::fs::File>> {
    let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    Ok(std::io::BufWriter::with_capacity(64 * 1024, f))
}

fn writer(
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    mut out: std::io::BufWriter<std::fs::File>,
    path: String,
    c: Arc<Counters>,
) {
    let mut last_summary = Instant::now();
    loop {
        let res = rx.recv_timeout(TICK);
        let disconnected = matches!(res, Err(RecvTimeoutError::Disconnected));
        if let Ok(line) = res {
            let _ = out.write_all(&line);
            // Drain what is queued, then fall through to housekeeping.
            while let Ok(line) = rx.try_recv() {
                let _ = out.write_all(&line);
            }
        }
        if last_summary.elapsed() >= Duration::from_secs(1) || disconnected {
            last_summary = Instant::now();
            summaries(&mut out, &c);
        }
        let _ = out.flush();
        if c.reopen.swap(false, Relaxed) {
            match open(&path) {
                Ok(o) => out = o,
                Err(e) => tracing::warn!(error = %e, %path, "access log reopen failed"),
            }
        }
        if disconnected {
            return;
        }
    }
}

/// Aggregated refusals and dropped lines since the last summary.
fn summaries(out: &mut impl std::io::Write, c: &Counters) {
    let last = c.last_refused.lock().ok().and_then(|l| *l);
    for (reason, n) in [
        ("allowlist", c.refused_allowlist.swap(0, Relaxed)),
        ("banned", c.refused_banned.swap(0, Relaxed)),
    ] {
        if n > 0 {
            let mut b = Vec::with_capacity(96);
            rfc3339(&mut b, SystemTime::now());
            let last = last.map(|ip| ip.to_string()).unwrap_or_else(|| "-".into());
            let _ = writeln!(Buf2(&mut b), " event=refused reason={reason} count={n} last={last}");
            let _ = out.write_all(&b);
        }
    }
    let dropped = c.dropped.swap(0, Relaxed);
    if dropped > 0 {
        let mut b = Vec::with_capacity(64);
        rfc3339(&mut b, SystemTime::now());
        let _ = writeln!(Buf2(&mut b), " event=dropped count={dropped}");
        let _ = out.write_all(&b);
    }
}

/// `io::Write` view used with `writeln!` on a byte vector.
struct Buf2<'a>(&'a mut Vec<u8>);

impl std::io::Write for Buf2<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC), civil-from-days (H. Hinnant) — no
/// date crate.
fn rfc3339(out: &mut Vec<u8>, t: SystemTime) {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let _ = write!(
        Buf(out),
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60,
        d.subsec_millis()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        let mut b = Vec::new();
        rfc3339(&mut b, SystemTime::UNIX_EPOCH);
        assert_eq!(b, b"1970-01-01T00:00:00.000Z");
        let mut b = Vec::new();
        // 2026-09-30T21:32:12.920Z
        rfc3339(&mut b, SystemTime::UNIX_EPOCH + Duration::from_millis(1_790_803_932_920));
        assert_eq!(String::from_utf8(b).unwrap(), "2026-09-30T21:32:12.920Z");
        let mut b = Vec::new();
        // Leap day 2024-02-29T12:00:00Z
        rfc3339(&mut b, SystemTime::UNIX_EPOCH + Duration::from_secs(1_709_208_000));
        assert_eq!(String::from_utf8(b).unwrap(), "2024-02-29T12:00:00.000Z");
    }

    #[test]
    fn values_are_sanitized() {
        let log = AccessLog::disabled();
        let l = log.line().kv("target", "a b\tc\nd").kv("empty", "").kv_bytes("url", b"/x y");
        let s = String::from_utf8(l.0).unwrap();
        assert!(s.ends_with(" target=a_b_c_d empty=- url=/x_y"), "{s}");
    }

    #[test]
    fn levels() {
        let off = AccessLog::disabled();
        assert!(!off.errors() && !off.all());
        let dir = std::env::temp_dir().join(format!("myproxy-log-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.log");
        let e = AccessLog::start(Level::Errors, f.to_str().unwrap(), TargetMode::Host).unwrap();
        assert!(e.errors() && !e.all());
        let a = AccessLog::start(Level::All, f.to_str().unwrap(), TargetMode::Url).unwrap();
        assert!(a.errors() && a.all() && a.target_mode() == TargetMode::Url);
        // Empty path = disabled whatever the level.
        assert!(!AccessLog::start(Level::All, "", TargetMode::Host).unwrap().errors());
    }

    #[test]
    fn batches_and_full_queue_drops_without_blocking() {
        let (tx, rx) = sync_channel::<Vec<u8>>(1);
        let log = AccessLog {
            level: Level::All,
            target: TargetMode::Host,
            tx: Some(tx),
            counters: Arc::new(Counters::default()),
        };
        // Lines accumulate locally: nothing handed over until a flush.
        for _ in 0..3 {
            log.emit(log.line().kv("k", 1));
        }
        assert!(rx.try_recv().is_err());
        log.flush_local();
        let batch = rx.try_recv().unwrap();
        assert_eq!(batch.iter().filter(|&&c| c == b'\n').count(), 3);
        // Queue of 1: the second batch is dropped, its lines counted.
        log.emit(log.line().kv("k", 1));
        log.flush_local();
        log.emit(log.line().kv("k", 2));
        log.emit(log.line().kv("k", 3));
        log.flush_local();
        assert_eq!(log.counters.dropped.load(Relaxed), 2);
    }
}
