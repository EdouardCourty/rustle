//! Load modes. Each mode owns its workload and its RESULT line; the CLI
//! dispatches here.

pub mod flood;
pub mod hold;
pub mod http;
pub mod sink;
pub mod upstream;

use crate::metrics::Metrics;

/// Reduced workload for the warmup run (unchanged shapes, fewer bytes).
pub(crate) fn warmup_of(cmd: &crate::Cmd) -> Option<crate::Cmd> {
    match cmd {
        crate::Cmd::Http {
            url,
            conns,
            duration,
            requests,
            no_keepalive,
        } => Some(crate::Cmd::Http {
            url: url.clone(),
            conns: *conns,
            // Time-budgeted warmup: a fifth of the measured budget, at
            // least 2 s. Count-based: a quarter of the requests.
            duration: duration.map(|d| d.clamp(2, 5)),
            requests: requests.map(|r| (r / 4).max(100)),
            no_keepalive: *no_keepalive,
        }),
        crate::Cmd::Flood {
            target,
            conns,
            duration: _,
            chunk,
        } => Some(crate::Cmd::Flood {
            target: target.clone(),
            conns: *conns,
            duration: 2,
            chunk: *chunk,
        }),
        crate::Cmd::Hold { .. } => None, // opening connections IS the test
        crate::Cmd::Sink { .. } | crate::Cmd::Upstream { .. } => None,
    }
}

pub(crate) async fn run_once(cli: &crate::Cli, cmd: &crate::Cmd) -> Metrics {
    match cmd {
        crate::Cmd::Http {
            url,
            conns,
            duration,
            requests,
            no_keepalive,
        } => {
            http::run_http(
                &cli.proxy,
                &cli.api_key,
                url,
                *conns,
                *duration,
                *requests,
                *no_keepalive,
                cli.threads(),
            )
            .await
        }
        crate::Cmd::Flood {
            target,
            conns,
            duration,
            chunk,
        } => {
            flood::run_flood(
                &cli.proxy,
                &cli.api_key,
                target,
                *conns,
                *duration,
                *chunk,
                cli.threads(),
            )
            .await
        }
        crate::Cmd::Hold {
            target,
            conns,
            duration,
            ramp,
        } => hold::run_hold(&cli.proxy, &cli.api_key, target, *conns, *duration, *ramp).await,
        crate::Cmd::Sink { .. } | crate::Cmd::Upstream { .. } => unreachable!(),
    }
}
