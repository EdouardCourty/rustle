//! The process: shipped example config, config errors (exit code and
//! message), signals (graceful SIGTERM drain, SIGHUP), admin endpoint.

use std::time::{Duration, Instant};

use anyhow::{ensure, Context};

use crate::clients::http1::{default_auth, get, Client};
use crate::harness::proxy::health;
use crate::harness::{payload, Config, Proxy};
use crate::runner::{scenario, Scenario};

fn example_config() -> anyhow::Result<String> {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../rustle/config.toml.example");
    std::fs::read_to_string(p).context("reading config.toml.example")
}

/// Invalid configs: (name, TOML, substring expected in stderr).
const INVALID: &[(&str, &str, &str)] = &[
    ("unknown-key", "[server]\nlisten = \"127.0.0.1:0\"\nlisen = 1\n", "unknown field"),
    ("unknown-section", "[serveur]\nlisten = \"127.0.0.1:0\"\n", "unknown field"),
    ("listen-hostname", "[server]\nlisten = \"localhost:8080\"\n", "expected \"IP:port\""),
    ("socks5-listen-port-only", "[server]\nlisten = \"127.0.0.1:0\"\n[socks5]\nlisten = \"1080\"\n", "invalid socks5 address"),
    ("tls-listen-garbage", "[server]\nlisten = \"127.0.0.1:0\"\n[tls]\nlisten = \"nope\"\ncert = \"x\"\nkey = \"y\"\n", "certificate"),
    ("tls-missing-cert", "[server]\nlisten = \"127.0.0.1:0\"\n[tls]\nlisten = \"127.0.0.1:0\"\ncert = \"/nonexistent/cert.pem\"\nkey = \"/nonexistent/key.pem\"\n", "reading certificate"),
    ("bad-duration", "[server]\nlisten = \"127.0.0.1:0\"\n[timeouts]\nidle = \"soon\"\n", "idle"),
    ("bad-log-level", "[server]\nlisten = \"127.0.0.1:0\"\n[log]\naccess = \"verbose\"\n", "verbose"),
    ("bad-cidr", "[server]\nlisten = \"127.0.0.1:0\"\n[filter]\ndeny = [\"10.0.0.0/99\"]\n", "10.0.0.0/99"),
    ("bad-type", "[server]\nlisten = \"127.0.0.1:0\"\nworkers = \"two\"\n", "workers"),
    ("not-toml", "this is = = not toml", "loading config"),
];

pub fn scenarios() -> Vec<Scenario> {
    let mut v = vec![
        scenario("process/example-config-starts", |env| async move {
            // The shipped example, listeners moved to free ports only.
            let text = example_config()?
                .replace("\"0.0.0.0:8080\"", "\"127.0.0.1:0\"")
                .replace("\"127.0.0.1:8181\"", "\"127.0.0.1:0\"");
            let cfg = Config::new();
            let p = Proxy::start_text(&cfg.dir, &text).await?;
            ensure!(health(p.admin.context("admin")?).await? == 200);
            // block_private = true in the example: a local origin is refused.
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/data/1")), false)
                .await?
                .expect(403)?;
            // Wrong key (403 closed the previous connection).
            let mut c = Client::plain(p.addr).await?;
            let req = crate::clients::http1::request(
                "GET",
                "http://example.com/",
                Some(&crate::clients::basic("u", "nope")),
                "",
                b"",
            );
            c.roundtrip(&req, false).await?.expect(407)?;
            drop(c);
            p.finish().await
        })
        .quick(),
        scenario("process/port-already-in-use", |_env| async move {
            let taken = std::net::TcpListener::bind("127.0.0.1:0")?;
            let cfg =
                Config::new().set("server", "listen", &format!("\"{}\"", taken.local_addr()?));
            let (code, err) = Proxy::start_fail(&cfg.dir, &cfg.render()).await?;
            ensure!(code != 0, "started on a busy port");
            ensure!(err.contains("binding listen"), "message: {err}");
            let _ = std::fs::remove_dir_all(&cfg.dir);
            Ok(())
        }),
        scenario("process/sigterm-drains-active-transfer", |env| async move {
            let mut p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.addr).await?;
            // ~2 s transfer, SIGTERM after 300 ms: it must complete.
            c.send(&get(&env.origin.url("/slow/1000000?ms=200")))
                .await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            p.signal(libc::SIGTERM);
            let r = c.response(false).await?;
            r.expect(200)?;
            payload::check(1_000_000, 1_000_000, &r.body)?;
            drop(c);
            let st = p.wait_exit(Duration::from_secs(8)).await?;
            ensure!(st.success(), "exit {st}");
            ensure!(p.stderr().contains("shutdown complete"));
            Ok(())
        }),
        scenario("process/sigterm-stops-accepting", |_env| async move {
            let mut p = Proxy::start(Config::new()).await?;
            let addr = p.addr;
            p.signal(libc::SIGTERM);
            let st = p.wait_exit(Duration::from_secs(8)).await?;
            ensure!(st.success());
            ensure!(
                tokio::net::TcpStream::connect(addr).await.is_err(),
                "still accepting after exit"
            );
            Ok(())
        }),
        scenario(
            "process/sigterm-with-idle-tunnel-exits-in-time",
            |env| async move {
                let mut p = Proxy::start(Config::new()).await?;
                let mut c = Client::plain(p.addr).await?;
                ensure!(
                    c.connect(&env.targets.echo.to_string(), Some(&default_auth()))
                        .await?
                        == 200
                );
                let t0 = Instant::now();
                p.signal(libc::SIGTERM);
                let st = p.wait_exit(Duration::from_secs(9)).await?;
                ensure!(st.success(), "exit {st}");
                ensure!(
                    t0.elapsed() < Duration::from_secs(7),
                    "drain took {:?}",
                    t0.elapsed()
                );
                drop(c);
                Ok(())
            },
        ),
        scenario("process/sighup-without-log-is-harmless", |env| async move {
            let p = Proxy::start(Config::new()).await?;
            p.signal(libc::SIGHUP);
            tokio::time::sleep(Duration::from_millis(200)).await;
            let mut c = Client::plain(p.addr).await?;
            c.roundtrip(&get(&env.origin.url("/data/1")), false)
                .await?
                .expect(200)?;
            drop(c);
            p.finish().await
        }),
        scenario("process/admin-disabled", |_env| async move {
            let p = Proxy::start(Config::new().set("server", "admin_listen", "\"\"")).await?;
            ensure!(p.admin.is_none(), "admin listener with admin_listen = \"\"");
            p.finish().await
        }),
        scenario("process/admin-404-on-other-paths", |_env| async move {
            let p = Proxy::start(Config::new()).await?;
            let mut c = Client::plain(p.admin.context("admin")?).await?;
            let r = c
                .roundtrip(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n", false)
                .await?;
            r.expect(404)?;
            drop(c);
            p.finish().await
        }),
    ];
    for &(name, toml, needle) in INVALID {
        v.push(scenario(
            format!("process/invalid-config/{name}"),
            move |_env| async move {
                let cfg = Config::new();
                let (code, err) = Proxy::start_fail(&cfg.dir, toml).await?;
                let _ = std::fs::remove_dir_all(&cfg.dir);
                ensure!(code != 0, "accepted invalid config");
                ensure!(err.contains(needle), "stderr lacks {needle:?}:\n{err}");
                ensure!(!err.contains("panicked"), "panic on invalid config:\n{err}");
                Ok(())
            },
        ));
    }
    v
}
