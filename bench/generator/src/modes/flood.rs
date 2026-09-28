//! CONNECT tunnels with bulk upload: measures raw Gbps through the tunnel
//! plus tunnel-establishment latencies.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::task::JoinSet;

use crate::connect::{connect_via_proxy, resolve_once};
use crate::metrics::{pcts_ms, Metrics};

// ---------------------------------------------------------------------------

pub(crate) async fn run_flood(
    proxy: &str,
    api_key: &str,
    target: &str,
    conns: u32,
    duration: u64,
    chunk: usize,
) -> Metrics {
    let deadline = Instant::now() + Duration::from_secs(duration);
    let target_owned = resolve_once(target)
        .await
        .unwrap_or_else(|e| panic!("resolve {target}: {e}"));
    let target: &str = &target_owned;
    let total_bytes = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let est_lats: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));

    let mut set = JoinSet::new();
    for _ in 0..conns {
        let proxy = proxy.to_string();
        let api_key = api_key.to_string();
        let target = target.to_string();
        let deadline = deadline;
        let total_bytes = total_bytes.clone();
        let errors = errors.clone();
        let est_lats = est_lats.clone();
        set.spawn(async move {
            let t0 = Instant::now();
            let mut stream = match connect_via_proxy(&proxy, &target, &api_key).await {
                Ok(s) => s,
                Err(e) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("flood connect error: {e}");
                    return;
                }
            };
            est_lats.lock().unwrap().push(t0.elapsed().as_secs_f64());
            let buf = vec![0x5au8; chunk.max(1024)];
            let mut sent: u64 = 0;
            while Instant::now() < deadline {
                match stream.write(&buf).await {
                    Ok(0) => break,
                    Ok(n) => sent += n as u64,
                    Err(_) => break,
                }
            }
            total_bytes.fetch_add(sent, Ordering::Relaxed);
        });
    }
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            eprintln!("task error: {e}");
        }
    }
    let bytes = total_bytes.load(Ordering::Relaxed);
    let lat = est_lats.lock().unwrap().clone();
    let (p50, p95, p99) = pcts_ms(&lat);
    Metrics {
        statics: vec![
            ("mode", "flood".to_string()),
            ("target", target.to_string()),
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
            ("errors", errors.load(Ordering::Relaxed) as f64),
        ],
    }
}
