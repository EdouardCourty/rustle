//! The system's curl, for interoperability with a real-world client.

use std::sync::OnceLock;

use anyhow::Context;
use tokio::process::Command;

pub struct Curl {
    pub version: String,
    /// `--proxy-http2` (curl >= 8.1).
    pub proxy_http2: bool,
}

/// Detected once; None when curl is missing.
pub fn detect() -> Option<&'static Curl> {
    static C: OnceLock<Option<Curl>> = OnceLock::new();
    C.get_or_init(|| {
        let out = std::process::Command::new("curl")
            .arg("--version")
            .output()
            .ok()?;
        let v = String::from_utf8_lossy(&out.stdout);
        let version = v.split_whitespace().nth(1)?.to_string();
        let help = std::process::Command::new("curl")
            .args(["--help", "all"])
            .output()
            .ok()?;
        Some(Curl {
            version,
            proxy_http2: String::from_utf8_lossy(&help.stdout).contains("--proxy-http2"),
        })
    })
    .as_ref()
}

pub struct CurlResult {
    pub exit: i32,
    pub http_code: u16,
    pub body: Vec<u8>,
    pub stderr: String,
}

/// Runs curl with `args` (+ silent, body to a file, status on stdout).
pub async fn run(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<CurlResult> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let out_file = dir.join(format!(
        "curl-{}.out",
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        Command::new("curl")
            .args(["-sS", "--max-time", "50", "-o"])
            .arg(&out_file)
            .args(["-w", "%{http_code}"])
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("curl timeout")??;
    let body = std::fs::read(&out_file).unwrap_or_default();
    let _ = std::fs::remove_file(&out_file);
    Ok(CurlResult {
        exit: out.status.code().unwrap_or(-1),
        http_code: String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0),
        body,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}
