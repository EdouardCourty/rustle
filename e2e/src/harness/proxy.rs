//! The proxy under test: the real release binary, started with a generated
//! TOML file, observed from outside (stderr, /proc, signals).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use super::certs;

pub const KEY: &str = "secret1";
pub const KEY2: &str = "secret2";

static BINARY: OnceLock<PathBuf> = OnceLock::new();
static KEEP: OnceLock<bool> = OnceLock::new();

pub fn set_binary(p: PathBuf, keep_dirs: bool) {
    let _ = BINARY.set(p);
    let _ = KEEP.set(keep_dirs);
}

fn binary() -> &'static Path {
    BINARY.get().expect("binary set by main")
}

tokio::task_local! {
    /// Output of every proxy started by the current scenario, so the
    /// runner can print it when the scenario fails.
    pub static OUTPUTS: Arc<Mutex<Vec<(String, Arc<Mutex<String>>)>>>;
}

/// A fresh temporary directory per proxy (config, certificates, logs).
fn temp_dir() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "myproxy-e2e-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

/// Proxy configuration: test defaults (every listener on 127.0.0.1:0, auth
/// on, private destinations allowed since every target is local), edited
/// with [`Self::set`]. Values are raw TOML.
pub struct Config {
    pub dir: PathBuf,
    sections: Vec<(String, Vec<(String, String)>)>,
}

impl Config {
    pub fn new() -> Self {
        let dir = temp_dir();
        let c = certs::localhost();
        std::fs::write(dir.join("cert.pem"), &c.cert_pem).expect("cert");
        std::fs::write(dir.join("key.pem"), &c.key_pem).expect("key");
        let q = |p: PathBuf| format!("{:?}", p.to_string_lossy());
        let mut cfg = Self {
            sections: Vec::new(),
            dir: dir.clone(),
        };
        cfg = cfg
            .set("server", "listen", "\"127.0.0.1:0\"")
            .set("server", "admin_listen", "\"127.0.0.1:0\"")
            .set("server", "workers", "1")
            .set("auth", "api_keys", &format!("[\"{KEY}\", \"{KEY2}\"]"))
            .set("timeouts", "connect", "\"2s\"")
            .set("timeouts", "idle", "\"10s\"")
            .set("filter", "block_private", "false")
            .set("tls", "listen", "\"127.0.0.1:0\"")
            .set("tls", "cert", &q(dir.join("cert.pem")))
            .set("tls", "key", &q(dir.join("key.pem")))
            .set("socks5", "listen", "\"127.0.0.1:0\"");
        cfg
    }

    /// Sets `[section] key = value` (value is raw TOML: `"text"`, `1`, `[..]`).
    pub fn set(mut self, section: &str, key: &str, value: &str) -> Self {
        let sec = match self.sections.iter().position(|(s, _)| s == section) {
            Some(i) => &mut self.sections[i].1,
            None => {
                self.sections.push((section.to_string(), Vec::new()));
                &mut self.sections.last_mut().unwrap().1
            }
        };
        match sec.iter_mut().find(|(k, _)| k == key) {
            Some(kv) => kv.1 = value.to_string(),
            None => sec.push((key.to_string(), value.to_string())),
        }
        self
    }

    /// Path inside the proxy's directory (access log file, etc.).
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for (s, kv) in &self.sections {
            out.push_str(&format!("[{s}]\n"));
            for (k, v) in kv {
                out.push_str(&format!("{k} = {v}\n"));
            }
            out.push('\n');
        }
        out
    }
}

/// A running proxy. Dropping it kills the process; [`Proxy::finish`] is
/// the clean end of a scenario (leak check + graceful shutdown).
pub struct Proxy {
    child: Child,
    pub dir: PathBuf,
    pub addr: std::net::SocketAddr,
    pub tls: Option<std::net::SocketAddr>,
    pub socks: Option<std::net::SocketAddr>,
    pub admin: Option<std::net::SocketAddr>,
    stderr: Arc<Mutex<String>>,
    stdout: Arc<Mutex<String>>,
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            for c in it.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Value of `key=` in a tracing line: `addr=1.2.3.4:5` or `tls=Some(..)`.
fn field(line: &str, key: &str) -> Option<std::net::SocketAddr> {
    let pat = format!(" {key}=");
    let v = line.split(&pat).nth(1)?.split(' ').next()?;
    let v = v
        .strip_prefix("Some(")
        .map_or(v, |v| v.trim_end_matches(')'));
    v.parse().ok()
}

fn collect(
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    sink: Arc<Mutex<String>>,
    ready: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let l = strip_ansi(&l);
            if let Some(tx) = &ready {
                if l.contains("myproxy listening") {
                    let _ = tx.send(l.clone());
                }
            }
            let mut s = sink.lock().unwrap();
            s.push_str(&l);
            s.push('\n');
        }
    });
}

impl Proxy {
    pub async fn start(cfg: Config) -> anyhow::Result<Self> {
        let text = cfg.render();
        Self::start_text(&cfg.dir, &text).await
    }

    /// Starts the binary on a raw config text written to `dir/config.toml`.
    pub async fn start_text(dir: &Path, text: &str) -> anyhow::Result<Self> {
        let path = dir.join("config.toml");
        std::fs::write(&path, text)?;
        let mut child = Command::new(binary())
            .arg(&path)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", binary().display()))?;
        let stderr = Arc::new(Mutex::new(String::new()));
        let stdout = Arc::new(Mutex::new(String::new()));
        let _ = OUTPUTS.try_with(|o| {
            o.lock()
                .unwrap()
                .push((dir.display().to_string(), stderr.clone()));
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        collect(child.stderr.take().unwrap(), stderr.clone(), Some(tx));
        collect(child.stdout.take().unwrap(), stdout.clone(), None);
        let line = tokio::select! {
            l = rx.recv() => l,
            st = child.wait() => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                bail!("proxy exited at startup ({:?}):\n{}", st?, stderr.lock().unwrap());
            }
            _ = tokio::time::sleep(Duration::from_secs(10)) => None,
        };
        let line = line.context("proxy did not report its listeners within 10 s")?;
        Ok(Self {
            child,
            dir: dir.to_path_buf(),
            addr: field(&line, "addr").context("no addr in startup line")?,
            tls: field(&line, "tls"),
            socks: field(&line, "socks5"),
            admin: field(&line, "admin"),
            stderr,
            stdout,
        })
    }

    /// Runs the binary on a config expected to be rejected: returns
    /// (exit code, stderr).
    pub async fn start_fail(dir: &Path, text: &str) -> anyhow::Result<(i32, String)> {
        let path = dir.join("config.toml");
        std::fs::write(&path, text)?;
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new(binary())
                .arg(&path)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("proxy kept running with an invalid config")??;
        Ok((
            out.status.code().unwrap_or(-1),
            strip_ansi(&String::from_utf8_lossy(&out.stderr)),
        ))
    }

    pub fn tls(&self) -> std::net::SocketAddr {
        self.tls.expect("TLS listener enabled")
    }

    pub fn socks(&self) -> std::net::SocketAddr {
        self.socks.expect("SOCKS5 listener enabled")
    }

    pub fn pid(&self) -> i32 {
        self.child.id().expect("running") as i32
    }

    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    pub fn stdout(&self) -> String {
        self.stdout.lock().unwrap().clone()
    }

    pub fn signal(&self, sig: i32) {
        // SAFETY: kill(2) on our own child's pid; no memory involved.
        unsafe {
            libc::kill(self.pid(), sig);
        }
    }

    /// Resident memory (VmRSS), KB.
    #[cfg(target_os = "linux")]
    pub fn rss_kb(&self) -> u64 {
        let s = std::fs::read_to_string(format!("/proc/{}/status", self.pid())).unwrap_or_default();
        s.lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
            .unwrap_or(0)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn rss_kb(&self) -> u64 {
        let o = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &self.pid().to_string()])
            .output();
        o.ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(0)
    }

    #[cfg(target_os = "linux")]
    pub fn fds(&self) -> usize {
        std::fs::read_dir(format!("/proc/{}/fd", self.pid()))
            .map(|d| d.count())
            .unwrap_or(0)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn fds(&self) -> usize {
        // lsof -F: one `f<fd>` line per open file.
        self.lsof(&[])
            .lines()
            .filter(|l| l.starts_with('f'))
            .count()
    }

    fn listen_ports(&self) -> Vec<u16> {
        [Some(self.addr), self.tls, self.socks]
            .into_iter()
            .flatten()
            .map(|a| a.port())
            .collect()
    }

    /// Client-side TCP connections the proxy holds: its sockets whose local
    /// port is one of its proxy listeners, LISTEN excluded. Pooled upstream
    /// sockets and cached pipes are not counted.
    pub fn client_sockets(&self) -> usize {
        self.client_sockets_in(|_| true)
    }

    /// Leaked client connections: closed by the client (CLOSE_WAIT) but
    /// still held by the proxy. Live clients' connections are not leaks.
    pub fn leaked_sockets(&self) -> usize {
        self.client_sockets_in(|close_wait| close_wait)
    }

    /// `f(is_close_wait)` selects among the non-LISTEN client sockets.
    #[cfg(target_os = "linux")]
    fn client_sockets_in(&self, f: impl Fn(bool) -> bool) -> usize {
        let pid = self.pid();
        let mut inodes = std::collections::HashSet::new();
        if let Ok(d) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            for e in d.flatten() {
                if let Ok(l) = std::fs::read_link(e.path()) {
                    let l = l.to_string_lossy();
                    if let Some(i) = l.strip_prefix("socket:[") {
                        inodes.insert(i.trim_end_matches(']').to_string());
                    }
                }
            }
        }
        let ports = self.listen_ports();
        let mut n = 0;
        for t in ["tcp", "tcp6"] {
            let t = std::fs::read_to_string(format!("/proc/{pid}/net/{t}")).unwrap_or_default();
            for l in t.lines().skip(1) {
                let c: Vec<&str> = l.split_whitespace().collect();
                if c.len() < 10 || c[3] == "0A" || !f(c[3] == "08") || !inodes.contains(c[9]) {
                    continue;
                }
                let port = c[1]
                    .rsplit(':')
                    .next()
                    .and_then(|p| u16::from_str_radix(p, 16).ok());
                if port.is_some_and(|p| ports.contains(&p)) {
                    n += 1;
                }
            }
        }
        n
    }

    /// Same, from `lsof -F` records (`n<local>-><peer>`, `TST=<state>`).
    #[cfg(not(target_os = "linux"))]
    fn client_sockets_in(&self, f: impl Fn(bool) -> bool) -> usize {
        let ports = self.listen_ports();
        let (mut n, mut local, mut state) = (0, None::<u16>, String::new());
        let mut flush = |local: &mut Option<u16>, state: &mut String| {
            if let Some(p) = local.take() {
                if ports.contains(&p) && state != "LISTEN" && f(state == "CLOSE_WAIT") {
                    n += 1;
                }
            }
            state.clear();
        };
        for l in self.lsof(&["-iTCP"]).lines() {
            if l.starts_with('f') {
                flush(&mut local, &mut state);
            } else if let Some(name) = l.strip_prefix('n') {
                // "127.0.0.1:8080->127.0.0.1:5555" or "*:8080"
                local = name
                    .split("->")
                    .next()
                    .and_then(|a| a.rsplit(':').next())
                    .and_then(|p| p.parse().ok());
            } else if let Some(st) = l.strip_prefix("TST=") {
                state = st.to_string();
            }
        }
        flush(&mut local, &mut state);
        n
    }

    #[cfg(not(target_os = "linux"))]
    fn lsof(&self, extra: &[&str]) -> String {
        let o = std::process::Command::new("lsof")
            .args(["-nP", "-F", "fnT", "-a", "-p", &self.pid().to_string()])
            .args(extra)
            .output();
        o.map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    /// Waits until `cond` holds (polling), or fails with `what`.
    pub async fn wait_for(
        &self,
        what: &str,
        timeout: Duration,
        mut cond: impl FnMut(&Self) -> bool,
    ) -> anyhow::Result<()> {
        let t0 = Instant::now();
        while !cond(self) {
            if t0.elapsed() > timeout {
                bail!("timeout ({timeout:?}) waiting for: {what}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }

    /// End of scenario: every connection its client closed must be released
    /// by the proxy, then SIGTERM must stop the process cleanly (exit 0, no
    /// panic).
    pub async fn finish(mut self) -> anyhow::Result<()> {
        self.wait_for(
            "connections closed by clients released by the proxy",
            Duration::from_secs(5),
            |p| p.leaked_sockets() == 0,
        )
        .await
        .with_context(|| {
            format!(
                "leak: {} client connections in CLOSE_WAIT",
                self.leaked_sockets()
            )
        })?;
        self.stop().await
    }

    /// SIGTERM, exit within the 5 s drain + margin, status 0, no panic.
    pub async fn stop(&mut self) -> anyhow::Result<()> {
        self.signal(libc::SIGTERM);
        let st = tokio::time::timeout(Duration::from_secs(8), self.child.wait())
            .await
            .context("proxy did not exit within 8 s of SIGTERM")??;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let err = self.stderr();
        anyhow::ensure!(!err.contains("panicked"), "proxy panicked:\n{err}");
        anyhow::ensure!(st.success(), "proxy exit status {st}:\n{err}");
        anyhow::ensure!(
            err.contains("shutdown complete"),
            "no clean shutdown line:\n{err}"
        );
        Ok(())
    }

    pub async fn wait_exit(
        &mut self,
        timeout: Duration,
    ) -> anyhow::Result<std::process::ExitStatus> {
        Ok(tokio::time::timeout(timeout, self.child.wait())
            .await
            .context("proxy still running")??)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        if !KEEP.get().copied().unwrap_or(false) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// `GET /health` on the admin listener; returns the status code.
pub async fn health(admin: std::net::SocketAddr) -> anyhow::Result<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(admin).await?;
    s.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
        .await?;
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut out)).await??;
    let line = String::from_utf8_lossy(&out);
    line.get(9..12)
        .and_then(|c| c.parse().ok())
        .context("bad /health response")
}
