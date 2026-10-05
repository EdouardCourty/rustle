//! Non-Linux stand-in for `splice.rs`: no pipes, no zero-copy. Every flow
//! stays on the adaptive user-space copy (`PipePair::take()` is `None`, so
//! `adaptive` never hands a flow over).

use tokio::net::TcpStream;

use super::adaptive;

/// Pipes refused so far: always 0 without a splice engine.
pub fn degraded_pipes() -> u64 {
    0
}

/// Never constructed: there is no pipe without splice.
pub struct PipePair;

impl PipePair {
    pub fn take() -> Option<Self> {
        None
    }
}

/// Unreachable (`PipePair::take()` is always `None`).
pub(super) async fn response(
    _up: &TcpStream,
    _client: &TcpStream,
    _limit: Option<u64>,
    _pipe: &PipePair,
    _idle: std::time::Duration,
) -> u64 {
    0
}

/// Tunnel relay, both directions, half-close propagated by the copy loops.
pub(super) async fn tunnel(
    client: TcpStream,
    up: TcpStream,
    idle: std::time::Duration,
    _client_ktls: bool,
) -> (u64, u64) {
    adaptive::bidirectional_io(client, up, idle).await
}
