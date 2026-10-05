//! `rustle init`: a ready-to-run config with a freshly generated API key.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use ring::rand::SecureRandom;

use super::user_config_path;

/// The shipped example, embedded at build time.
const EXAMPLE: &str = include_str!("../../config.toml.example");

/// 192 random bits, hex.
fn generate_key() -> anyhow::Result<String> {
    let mut b = [0u8; 24];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| anyhow::anyhow!("system random generator failed"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// The example with its placeholder `api_keys` array replaced by `key`.
fn render(key: &str) -> anyhow::Result<String> {
    let start = EXAMPLE
        .find("api_keys = [")
        .context("example config has no api_keys array")?;
    let end = start
        + EXAMPLE[start..]
            .find(']')
            .context("example config: unterminated api_keys")?
        + 1;
    Ok(format!(
        "{}api_keys = [\n    \"{key}\",\n]{}",
        &EXAMPLE[..start],
        &EXAMPLE[end..]
    ))
}

pub fn run(path: Option<PathBuf>, force: bool) -> anyhow::Result<i32> {
    let path = match path {
        Some(p) => p,
        None => user_config_path()?,
    };
    if path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    let key = generate_key()?;
    let text = render(&key)?;
    write_private(&path, &text)?;
    // The result must load: a broken template would otherwise surface at
    // the first start.
    crate::config::Config::load(&path).context("generated config does not load")?;
    println!("wrote {}", path.display());
    println!("API key: {key}");
    println!(
        "\nuse it as the proxy password (any user name), e.g.\n  \
         curl -x http://me:{key}@127.0.0.1:8080 https://example.com\n\n\
         next: rustle run            # foreground\n      \
         rustle service enable   # at boot / login"
    );
    Ok(0)
}

/// Creates parent directories, writes with mode 0600 (it holds the key).
fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_config_loads_with_the_generated_key() {
        let key = generate_key().unwrap();
        assert_eq!(key.len(), 48);
        let text = render(&key).unwrap();
        assert!(text.contains(&format!("\"{key}\"")));
        let dir = std::env::temp_dir().join(format!("rustle-init-test-{}", std::process::id()));
        let p = dir.join("sub").join("config.toml");
        run(Some(p.clone()), false).unwrap();
        assert!(run(Some(p.clone()), false).is_err(), "no silent overwrite");
        run(Some(p.clone()), true).unwrap();
        let cfg = crate::config::Config::load(&p).unwrap();
        assert_eq!(cfg.auth.api_keys.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(p.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
