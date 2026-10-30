//! CONNECT tunnels with bulk upload: measures raw Gbps through the tunnel
//! plus tunnel-establishment latencies. Shared-nothing threads like the
//! HTTP mode (see `rt`).

use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::task::JoinSet;

use crate::connect::{connect_via_proxy, resolve_once};
use crate::metrics::{Hist, Metrics};
use crate::rt::{run_threads, share};

pub(crate) async fn run_flood(
    proxy: &str,
    api_key: &str,
    target: &str,
    conns: u32,
    duration: u64,
    chunk: usize,
    threads: usize,
) -> Metrics {
    let deadline = Instant::now() + Duration::from_secs(duration);
    let target_owned = resolve_once(target)
        .await
        .unwrap_or_else(|e| panic!("resolve {target}: {e}"));
    let (proxy_s, key_s, target_s) = (proxy.to_string(), api_key.to_string(), target_owned.clone());
    let parts = tokio::task::spawn_blocking(move || {
        run_threads(threads, move |ti| {
            let (proxy, key, target) = (proxy_s.clone(), key_s.clone(), target_s.clone());
            let mine = share(conns, threads, ti);
            async move {
                let mut set = JoinSet::new();
                for _ in 0..mine {
                    let (proxy, key, target) = (proxy.clone(), key.clone(), target.clone());
                    set.spawn(async move {
                        let t0 = Instant::now();
                        let mut stream = match connect_via_proxy(&proxy, &target, &key).await {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("flood connect error: {e}");
                                return (0u64, 1u64, None);
                            }
                        };
                        let est = t0.elapsed();
                        let buf = vec![0x5au8; chunk.max(1024)];
                        let mut sent: u64 = 0;
                        while Instant::now() < deadline {
                            match stream.write(&buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => sent += n as u64,
                            }
                        }
                        (sent, 0, Some(est))
                    });
                }
                let (mut bytes, mut errors, mut hist) = (0u64, 0u64, Hist::new());
                while let Some(res) = set.join_next().await {
                    match res {
                        Ok((b, e, est)) => {
                            bytes += b;
                            errors += e;
                            if let Some(d) = est {
                                hist.record(d);
                            }
                        }
                        Err(e) => eprintln!("task error: {e}"),
                    }
                }
                (bytes, errors, hist)
            }
        })
    })
    .await
    .expect("worker threads");
    let (mut bytes, mut errors, mut hist) = (0u64, 0u64, Hist::new());
    for (b, e, h) in parts {
        bytes += b;
        errors += e;
        hist.merge(&h);
    }
    let (p50, p95, p99) = hist.pcts_ms();
    Metrics {
        statics: vec![
            ("mode", "flood".to_string()),
            ("target", target_owned),
            ("conns", conns.to_string()),
            ("duration", duration.to_string()),
            ("chunk", chunk.to_string()),
        ],
        nums: vec![
            ("gbps", bytes as f64 * 8.0 / duration as f64 / 1e9),
            ("mb", bytes as f64 / 1e6),
            ("p50_ms", p50),
            ("p95_ms", p95),
            ("p99_ms", p99),
            ("errors", errors as f64),
        ],
    }
}
