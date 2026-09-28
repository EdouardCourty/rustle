//! bench-client: the load generator. Parses the CLI, runs warmup +
//! measured rounds, prints machine-parsable `RESULT` lines.

mod connect;
mod metrics;
mod modes;

use clap::{Parser, Subcommand};
use metrics::median_metrics;

/// Read /proc/self/statm resident field, in MB (Linux container target).
fn rss_pages() -> f64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse().ok())
        .map(|pages: f64| pages * 4096.0 / 1024.0 / 1024.0)
        .unwrap_or(-1.0)
}

#[derive(Parser)]
#[command(
    name = "bench-client",
    about = "Load generator for the proxy benchmark"
)]
struct Cli {
    /// Proxy address host:port.
    #[arg(long, default_value = "proxy:8080")]
    proxy: String,
    /// API key sent as Basic auth password (user is always "bench").
    #[arg(long, default_value = "secret1")]
    api_key: String,
    /// Number of measured runs; the median RESULT is reported.
    #[arg(long, default_value_t = 3)]
    repeat: u32,
    /// Disable the warmup run.
    #[arg(long)]
    no_warmup: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Clone)]
enum Cmd {
    /// Plain HTTP GET requests through the proxy (absolute-form).
    Http {
        /// Target URL, e.g. http://upstream:8000/1kb
        url: String,
        /// Concurrent connections.
        #[arg(long)]
        conns: u32,
        /// Total number of requests.
        #[arg(long)]
        requests: u64,
        /// Open a fresh connection for every request.
        #[arg(long)]
        no_keepalive: bool,
    },
    /// CONNECT tunnels with bulk upload; measures Gbps through the tunnel.
    Flood {
        /// Target host:port (TCP sink).
        target: String,
        /// Concurrent tunnels.
        #[arg(long)]
        conns: u32,
        /// Duration in seconds.
        #[arg(long, default_value_t = 10)]
        duration: u64,
        /// Write chunk size in bytes.
        #[arg(long, default_value_t = 65536)]
        chunk: usize,
    },
    /// Open N CONNECT tunnels and hold them for a duration.
    Hold {
        /// Target host:port (TCP sink).
        target: String,
        /// Total tunnels to open.
        #[arg(long)]
        conns: u32,
        /// Hold duration in seconds.
        #[arg(long, default_value_t = 10)]
        duration: u64,
        /// Concurrent connect attempts (ramp).
        #[arg(long, default_value_t = 4096)]
        ramp: u32,
    },
    /// TCP discard server (sink): accepts connections and drops all bytes.
    /// No process fork, no per-connection allocation.
    Sink {
        /// Listen address.
        #[arg(default_value = "0.0.0.0:9000")]
        listen: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    // Own RSS (MB): the hold/flood generators spawn tens of thousands of
    // tasks; knowing the harness footprint matters on small boxes (the
    // 4 GB dev instance once OOM'd mid-matrix). Read at start …
    let own_rss_start = rss_pages();
    if let Cmd::Sink { listen } = &cli.cmd {
        modes::sink::run_sink(listen).await;
    } else {
        if !cli.no_warmup {
            if let Some(w) = modes::warmup_of(&cli.cmd) {
                eprintln!("--- warmup");
                let _ = modes::run_once(&cli, &w).await;
            }
        }
        let mut runs = Vec::new();
        for k in 1..=cli.repeat {
            let m = modes::run_once(&cli, &cli.cmd).await;
            println!(
                "{}",
                m.line(&format!("run={k} own_rss_mb={own_rss_start:.1}"))
            );
            runs.push(m);
        }
        // … and again after the runs: the retained footprint (tasks, buffers
        // kept by the allocator) is the load the client puts on the box.
        let own_rss_end = rss_pages();
        println!(
            "{}",
            median_metrics(runs).line(&format!(
                "median own_rss_mb={own_rss_start:.1} own_rss_end={own_rss_end:.1}"
            ))
        );
    }
}
