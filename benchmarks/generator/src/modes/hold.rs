//! Mass tunnel holding: open N tunnels, hold them for a duration. The
//! ramp semaphore gates only the CONNECT phase — released as soon as the
//! tunnel is established — so the open rate reflects accept capacity.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::connect::{connect_via_proxy, resolve_once};
use crate::metrics::{pcts_ms, Metrics};

// ---------------------------------------------------------------------------

pub(crate) async fn run_hold(
    proxy: &str,
    api_key: &str,
    target: &str,
    conns: u32,
    duration: u64,
    ramp: u32,
) -> Metrics {
    let deadline = Instant::now() + Duration::from_secs(duration);
    let started = Instant::now();
    let target_owned = resolve_once(target)
        .await
        .unwrap_or_else(|e| panic!("resolve {target}: {e}"));
    let target: &str = &target_owned;
    let opened = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let lats: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    // Instant (as unix-ish ms) of the most recent successful open, so the
    // open rate uses the open window, not the total hold duration.
    let last_open_ms = Arc::new(AtomicU64::new(0));
    let sem = Arc::new(Semaphore::new(ramp as usize));

    let mut set = JoinSet::new();
    for _ in 0..conns {
        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };
        let proxy = proxy.to_string();
        let api_key = api_key.to_string();
        let target = target.to_string();
        let opened = opened.clone();
        let failed = failed.clone();
        let lats = lats.clone();
        let last_open_ms = last_open_ms.clone();
        set.spawn(async move {
            // The permit gates the CONNECT phase only, not the hold:
            // release it as soon as the tunnel is established.
            let _permit = permit;
            let t0 = Instant::now();
            let res = connect_via_proxy(&proxy, &target, &api_key).await;
            drop(_permit);
            match res {
                Ok(stream) => {
                    lats.lock().unwrap().push(t0.elapsed().as_secs_f64());
                    opened.fetch_add(1, Ordering::Relaxed);
                    last_open_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    tokio::time::sleep_until(deadline.into()).await;
                    drop(stream);
                }
                Err(e) => {
                    eprintln!("hold connect error: {e}");
                    failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            eprintln!("task error: {e}");
        }
    }
    let opened_n = opened.load(Ordering::Relaxed);
    let failed_n = failed.load(Ordering::Relaxed);
    let open_window_s = last_open_ms.load(Ordering::Relaxed) as f64 / 1000.0;
    let total_s = started.elapsed().as_secs_f64();
    let lat = lats.lock().unwrap().clone();
    let (p50, p95, p99) = pcts_ms(&lat);
    Metrics {
        statics: vec![
            ("mode", "hold".to_string()),
            ("target", target.to_string()),
            ("conns", conns.to_string()),
            ("duration", duration.to_string()),
            ("ramp", ramp.to_string()),
        ],
        nums: vec![
            ("opened", opened_n as f64),
            ("failed", failed_n as f64),
            ("rate", opened_n as f64 / open_window_s.max(0.001)),
            ("open_s", open_window_s),
            ("elapsed_s", total_s),
            ("p50_ms", p50),
            ("p95_ms", p95),
            ("p99_ms", p99),
        ],
    }
}
