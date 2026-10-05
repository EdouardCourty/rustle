//! bench-client: the load generator. Parses the CLI, runs warmup +
//! measured rounds, prints machine-parsable `RESULT` lines.

mod connect;
mod metrics;
mod modes;
mod rt;

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

/// utime+stime of this process, in CPU ticks (CLK_TCK=100, same
/// convention as the harness' proxy sampler). The generator/upstream/sink
/// CPU is the half of the benchmark nobody published — and at high rps it
/// can be the real ceiling, not the proxy under test.
fn cpu_ticks() -> u64 {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            // comm may contain spaces and parens — parse after the last ')'.
            let after = s.rsplit_once(')')?.1;
            let mut it = after.split_whitespace();
            // Fields after ')': state(3), ppid(4) … utime(14) is the 12th
            // item here, stime(15) the 13th.
            let utime: u64 = it.nth(11)?.parse().ok()?;
            let stime: u64 = it.next()?.parse().ok()?;
            Some(utime + stime)
        })
        .unwrap_or(0)
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
    /// Run the warmup only, then exit (the harness measures the proxy's
    /// CPU over the measured runs alone).
    #[arg(long)]
    warmup_only: bool,
    /// Worker threads, one `current_thread` runtime each (default: the CPUs
    /// this process may use — size it to the cores given to the generator,
    /// or it saturates before a multi-core proxy does).
    #[arg(long)]
    threads: Option<usize>,
    /// Talk TLS to the proxy (HTTPS proxy listener).
    #[arg(long)]
    proxy_tls: bool,
    /// HTTP/2 to the proxy (implies --proxy-tls; ALPN h2).
    #[arg(long)]
    proxy_h2: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

impl Cli {
    fn threads(&self) -> usize {
        self.threads
            .unwrap_or_else(|| {
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
            })
            .max(1)
    }
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
        /// Stop after this many seconds (time-budgeted mode). Mutually
        /// exclusive with --requests.
        #[arg(long, conflicts_with = "requests")]
        duration: Option<u64>,
        /// Stop after this many requests. Ignored when --duration is set.
        #[arg(long)]
        requests: Option<u64>,
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
    /// Static origin server: /1kb /10kb /1mb /10mb from memory, HTTP/1.1
    /// keep-alive (replaces nginx as the benchmark origin).
    Upstream {
        /// Listen address.
        #[arg(default_value = "0.0.0.0:8000")]
        listen: String,
    },
}

// Standalone binary (not part of the rustle workspace build profile):
// the sink mode spawns tens of thousands of tasks at 25k tunnels — the
// default 2 MB stacks cost ~25 KB touched per task (627 MB RSS at 25k,
// measured) and OOM-pressure the 4 GB box. A hand-built runtime with
// small stacks (like rustle's) cuts that ~4x.
fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Servers run their own per-thread runtimes (never returns).
    match &cli.cmd {
        Cmd::Sink { listen } => modes::sink::run_sink(listen, cli.threads()),
        Cmd::Upstream { listen } => modes::upstream::run_upstream(listen, cli.threads()),
        _ => {}
    }
    // Load modes: the work runs on per-thread runtimes (modes::*); this
    // one only orchestrates (hold uses it for its tasks).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cli.threads())
        .enable_all()
        .thread_stack_size(128 * 1024)
        .build()?;
    rt.block_on(amain(cli));
    Ok(())
}

async fn amain(cli: Cli) {
    if cli.proxy_tls || cli.proxy_h2 {
        connect::enable_proxy_tls(cli.proxy_h2);
    }
    // Own RSS (MB): the hold/flood generators spawn tens of thousands of
    // tasks; knowing the harness footprint matters on small boxes (the
    // 4 GB dev instance once OOM'd mid-matrix). Read at start …
    let own_rss_start = rss_pages();
    {
        if !cli.no_warmup {
            if let Some(w) = modes::warmup_of(&cli.cmd) {
                eprintln!("--- warmup");
                let _ = modes::run_once(&cli, &w).await;
            }
        }
        if cli.warmup_only {
            return;
        }
        // Generator CPU over the measured runs only (warmup excluded),
        // attributed per run like the proxy's cpu_s_per_run.
        let gen_ticks0 = cpu_ticks();
        let mut runs = Vec::new();
        for k in 1..=cli.repeat {
            let m = modes::run_once(&cli, &cli.cmd).await;
            println!(
                "{}",
                m.line(&format!("run={k} own_rss_mb={own_rss_start:.1}"))
            );
            runs.push(m);
        }
        let gen_ticks1 = cpu_ticks();
        let gen_cpu_per_run =
            gen_ticks1.saturating_sub(gen_ticks0) as f64 / (100.0 * cli.repeat.max(1) as f64);
        // … and again after the runs: the retained footprint (tasks, buffers
        // kept by the allocator) is the load the client puts on the box.
        let own_rss_end = rss_pages();
        println!(
            "{}",
            median_metrics(runs).line(&format!(
                "median own_rss_mb={own_rss_start:.1} own_rss_end={own_rss_end:.1} \
                 gen_cpu_s_per_run={gen_cpu_per_run:.2}"
            ))
        );
    }
}
