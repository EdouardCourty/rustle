mod accesslog;
mod auth;
mod cli;
mod cmd;
mod config;
mod dns;
mod http;
#[cfg(target_os = "linux")]
mod ktls_offload;
mod policy;
mod proxy;
mod relay;
mod server;
mod stream;
mod sys;
mod tls;
mod upstream;

// Global allocator selection (compile-time feature; glibc by default).
// See rustle/Cargo.toml [features]: jemalloc | mimalloc. The benchmark
// measures RSS with each — per-connection alloc/free churn keeps pages
// in RSS under glibc, the alternatives return them sooner.
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    let code = match cli.dispatch() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("rustle: {e:#}");
            1
        }
    };
    std::process::exit(code);
}
