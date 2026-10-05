//! `rustle update`: replace the running binary with the latest release.
//!
//! The release contract (shared with `install.sh`): per target an asset
//! `rustle-<target>.tar.gz` (containing `rustle`), a `SHA256SUMS` file and
//! a `VERSION` file (`X.Y.Z`). Downloads shell out to `curl`/`wget` (no
//! HTTPS client is linked in), the checksum is verified here (ring), the
//! new binary must run (`--version`) before it replaces the old one, and
//! the replacement is an atomic rename in the executable's directory.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

use crate::cli::{TARGET, VERSION};

const REPO: &str = "EdouardCourty/rustle";

fn base_url(version: Option<&str>) -> String {
    if let Ok(b) = std::env::var("RUSTLE_BASE_URL") {
        return b.trim_end_matches('/').to_string();
    }
    match version {
        Some(v) => format!(
            "https://github.com/{REPO}/releases/download/v{}",
            v.trim_start_matches('v')
        ),
        None => format!("https://github.com/{REPO}/releases/latest/download"),
    }
}

/// `X.Y.Z` (optionally `v`-prefixed, pre-release suffix ignored).
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let v = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    it.next().is_none().then_some(v)
}

fn fetch(url: &str, dest: &Path) -> anyhow::Result<()> {
    let d = dest.display().to_string();
    let curl = std::process::Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o", &d, url])
        .status();
    match curl {
        Ok(s) if s.success() => return Ok(()),
        Ok(_) => bail!("download failed: {url}"),
        Err(_) => {} // no curl: try wget
    }
    let wget = std::process::Command::new("wget")
        .args(["-q", "-O", &d, url])
        .status()
        .context("neither curl nor wget is installed")?;
    if !wget.success() {
        bail!("download failed: {url}");
    }
    Ok(())
}

fn sha256_hex(path: &Path) -> anyhow::Result<String> {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(ctx
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The hash listed for `asset` in a `sha256sum`-style file.
fn expected_hash(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let hash = it.next()?;
        let name = it.next()?.trim_start_matches('*');
        (name == asset).then(|| hash.to_ascii_lowercase())
    })
}

/// Removes the work directory on every exit path.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(clap::Args)]
pub struct Args {
    /// Only report whether an update is available
    #[arg(long)]
    pub check: bool,
    /// Install this version (e.g. v0.2.0) instead of the latest
    #[arg(long, value_name = "VERSION")]
    pub version: Option<String>,
}

pub fn run(args: Args) -> anyhow::Result<i32> {
    let Args { check, version } = args;
    let work = TempDir(std::env::temp_dir().join(format!("rustle-update-{}", std::process::id())));
    std::fs::create_dir_all(&work.0)?;
    let base = base_url(version.as_deref());

    // Which version is on offer.
    let target_version = match &version {
        Some(v) => v.trim_start_matches('v').to_string(),
        None => {
            let f = work.0.join("VERSION");
            fetch(&format!("{base}/VERSION"), &f).context("looking up the latest version")?;
            std::fs::read_to_string(&f)?.trim().to_string()
        }
    };
    let new = parse_version(&target_version)
        .with_context(|| format!("unrecognized version `{target_version}`"))?;
    let cur = parse_version(VERSION).context("own version is not X.Y.Z")?;
    println!("installed: {VERSION}   available: {target_version}");
    if version.is_none() && new <= cur {
        println!("already up to date");
        return Ok(0);
    }
    if check {
        println!("update available: run `rustle update`");
        return Ok(0);
    }

    let asset = format!("rustle-{TARGET}.tar.gz");
    let archive = work.0.join(&asset);
    let sums = work.0.join("SHA256SUMS");
    println!("downloading {asset}");
    fetch(&format!("{base}/{asset}"), &archive)?;
    fetch(&format!("{base}/SHA256SUMS"), &sums)?;
    let want = expected_hash(&std::fs::read_to_string(&sums)?, &asset)
        .with_context(|| format!("{asset} is not listed in SHA256SUMS"))?;
    let got = sha256_hex(&archive)?;
    if got != want {
        bail!("checksum mismatch for {asset}\n  expected {want}\n  got      {got}");
    }
    println!("checksum ok");

    let ex = work.0.join("extract");
    std::fs::create_dir_all(&ex)?;
    let st = std::process::Command::new("tar")
        .args([
            "-xzf",
            &archive.display().to_string(),
            "-C",
            &ex.display().to_string(),
        ])
        .status()
        .context("running tar")?;
    if !st.success() {
        bail!("could not extract {asset}");
    }
    let fresh = ex.join("rustle");
    // The new binary must run on this machine before it replaces ours.
    let (code, out) = super::run_capture(&fresh.display().to_string(), &["--version"])
        .context("the downloaded binary does not run on this system")?;
    if code != 0 || !out.contains(&target_version) {
        bail!(
            "downloaded binary reports `{}`, expected {target_version}",
            out.trim()
        );
    }

    let exe = std::env::current_exe()?.canonicalize()?;
    replace_exe(&fresh, &exe)?;
    println!("updated {} → {target_version}", exe.display());
    if let Some(what) = super::service::restart_if_installed() {
        println!("restarted the {what}");
    }
    Ok(0)
}

/// Copies `fresh` next to `exe`, then renames it over: atomic, and a
/// running process keeps its (now unlinked) image.
fn replace_exe(fresh: &Path, exe: &Path) -> anyhow::Result<()> {
    let dir = exe.parent().context("executable has no directory")?;
    let tmp = dir.join(format!(".rustle.new.{}", std::process::id()));
    let res = (|| -> std::io::Result<()> {
        std::fs::copy(fresh, &tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp, exe)
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            bail!(
                "cannot write to {}: re-run with the right privileges (e.g. `sudo rustle update`)",
                dir.display()
            );
        }
        return Err(e).context("replacing the executable");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10.0\n"), Some((0, 10, 0)));
        assert_eq!(parse_version("1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), None);
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
    }

    #[test]
    fn checksum_lookup() {
        let sums = "AAAA11  rustle-x86_64-unknown-linux-gnu.tar.gz\nbbbb22 *rustle-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            expected_hash(sums, "rustle-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("aaaa11")
        );
        assert_eq!(
            expected_hash(sums, "rustle-aarch64-apple-darwin.tar.gz").as_deref(),
            Some("bbbb22")
        );
        assert_eq!(expected_hash(sums, "nope"), None);
    }

    #[test]
    fn sha256_of_a_file() {
        let p = std::env::temp_dir().join(format!("rustle-sha-{}", std::process::id()));
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_hex(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(p);
    }
}
