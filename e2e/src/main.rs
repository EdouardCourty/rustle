//! rustle black-box functional suite.
//!
//! Starts the real release binary (`target/release/rustle`) once per
//! scenario with a generated config, against local origins and targets,
//! and drives it with real clients (HTTP/1.1, HTTPS, HTTP/2, SOCKS5, curl).
//!
//! ```text
//! cargo run --profile e2e -p e2e -- [FILTER...] [--quick] [--jobs N] [--list] [--keep] [--bin PATH]
//! ```
//! FILTER: substrings of scenario names (any match). See e2e/README.md.

mod clients;
mod harness;
mod runner;
mod suites;

use std::path::PathBuf;

use anyhow::Context;

fn main() -> anyhow::Result<()> {
    let mut filters = Vec::new();
    let mut quick = false;
    let mut list = false;
    let mut keep = false;
    let mut jobs = std::thread::available_parallelism().map_or(2, |n| n.get());
    let mut bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/release/rustle");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--quick" => quick = true,
            "--list" => list = true,
            "--keep" => keep = true,
            "--jobs" | "-j" => jobs = args.next().context("--jobs N")?.parse()?,
            "--bin" => bin = args.next().context("--bin PATH")?.into(),
            "-h" | "--help" => {
                println!(
                    "usage: e2e [FILTER...] [--quick] [--jobs N] [--list] [--keep] [--bin PATH]"
                );
                return Ok(());
            }
            f if f.starts_with('-') => anyhow::bail!("unknown option {f}"),
            f => filters.push(f.to_string()),
        }
    }

    suites::set_quick(quick);
    let mut all = suites::all();
    all.retain(|s| {
        (!quick || s.quick) && (filters.is_empty() || filters.iter().any(|f| s.name.contains(f)))
    });
    if list {
        for s in &all {
            println!("{}{}", s.name, if s.quick { "  [quick]" } else { "" });
        }
        println!("{} scenarios", all.len());
        return Ok(());
    }
    anyhow::ensure!(!all.is_empty(), "no scenario matches {filters:?}");

    let bin = bin.canonicalize().with_context(|| {
        format!(
            "proxy binary {} (cargo build --release -p rustle)",
            bin.display()
        )
    })?;
    harness::proxy::set_binary(bin.clone(), keep);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let ok = rt.block_on(async {
        let env = harness::Env::start().await?;
        let curl = clients::curl::detect();
        println!(
            "binary {}\n{} scenarios, {jobs} jobs, kTLS {}, curl {}\n",
            bin.display(),
            all.len(),
            if env.ktls { "available" } else { "unavailable" },
            curl.map_or("missing".to_string(), |c| format!(
                "{}{}",
                c.version,
                if c.proxy_http2 { " (proxy-http2)" } else { "" }
            )),
        );
        anyhow::Ok(runner::run(env, all, jobs).await)
    })?;
    rt.shutdown_timeout(std::time::Duration::from_millis(200));
    std::process::exit(if ok { 0 } else { 1 });
}
