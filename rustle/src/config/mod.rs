use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

fn de_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    humantime::parse_duration(&s).map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub limits: LimitsConfig,
    pub timeouts: TimeoutsConfig,
    pub dns: DnsConfig,
    pub http: HttpConfig,
    pub tls: TlsConfig,
    pub filter: FilterConfig,
    pub socks5: Socks5Config,
    pub log: LogConfig,
}

/// Access log (see `accesslog`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// off | errors | all.
    pub access: crate::accesslog::Level,
    /// file | stdout.
    pub output: crate::accesslog::Output,
    /// Log file when `output = "file"` (empty = no access log).
    pub file: String,
    /// host | url: how a plain-HTTP request target is written.
    pub target: crate::accesslog::TargetMode,
}

/// Destination policy (see `policy`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilterConfig {
    /// Refuse loopback, private, link-local (cloud metadata), CGNAT,
    /// multicast, reserved and IPv6 ULA/link-local destinations — checked
    /// on the resolved address.
    pub block_private: bool,
    /// Extra refused networks (CIDR).
    pub deny: Vec<String>,
    /// Destination ports allowed for CONNECT, HTTP and SOCKS5 (empty =
    /// every port).
    pub allowed_ports: Vec<u16>,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            block_private: true,
            deny: Vec::new(),
            allowed_ports: Vec::new(),
        }
    }
}

/// SOCKS5 listener (TCP CONNECT and UDP ASSOCIATE; the API key is the
/// password).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Socks5Config {
    /// `"IP:port"` (literal IP, e.g. `"127.0.0.1:1080"`). Empty = disabled.
    pub listen: String,
}

/// HTTPS proxy listener (TLS between client and proxy), next to the plain
/// one. Disabled while `listen` is empty.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    pub listen: String,
    /// PEM certificate chain and private key.
    pub cert: String,
    pub key: String,
    /// Cap on the TLS handshake (a stalled handshake holds a slot).
    #[serde(deserialize_with = "de_duration")]
    pub handshake_timeout: Duration,
    /// Offer HTTP/2 through ALPN (clients that do not ask keep HTTP/1.1).
    pub http2: bool,
    /// Kernel TLS after the handshake (when the kernel supports it, probed
    /// at startup). Off by default: measured faster for CONNECT tunnels
    /// and lighter in memory, slower for HTTP bulk and per connection
    /// (bench/RESULTS.md, M17).
    pub ktls: bool,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            listen: String::new(),
            cert: String::new(),
            key: String::new(),
            handshake_timeout: Duration::from_secs(10),
            http2: true,
            ktls: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DnsConfig {
    /// TTL of positive cache entries. 0 disables caching.
    #[serde(deserialize_with = "de_duration")]
    pub ttl: Duration,
    /// Cache entry cap. When exceeded the cache is wiped (simple, bounded).
    pub max_entries: usize,
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(60),
            max_entries: 4096,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    /// Shards: one thread + `current_thread` runtime + SO_REUSEPORT
    /// listener + upstream pool each. 0 = one per available CPU.
    pub workers: usize,
    /// Pin shard i to the i-th CPU of the process affinity (cpuset-aware).
    pub pin_cores: bool,
    /// Admin/health listener (GET /health -> 200, no auth). Bind to
    /// 127.0.0.1 on a public VPS. Empty string disables it.
    pub admin_listen: String,
    /// SO_SNDBUF/SO_RCVBUF on every proxied socket (client and upstream).
    /// 0 = kernel autotuning (tcp_rmem/tcp_wmem). A fixed size disables
    /// autotuning and is capped by net.core.rmem_max/wmem_max.
    pub socket_buffer_bytes: usize,
    /// Unix socket for local control commands (`rustle reload`). `"auto"`:
    /// `/run/rustle/rustle.sock` as root, else `$XDG_RUNTIME_DIR/rustle.sock`,
    /// else `rustle.sock` next to the user config. Empty = disabled.
    pub control_socket: String,
}

impl ServerConfig {
    /// Where the control socket lives (None = disabled).
    pub fn control_socket_path(&self) -> Option<std::path::PathBuf> {
        use std::path::PathBuf;
        match self.control_socket.as_str() {
            "" => None,
            "auto" if crate::sys::is_root() => Some(PathBuf::from("/run/rustle/rustle.sock")),
            "auto" => match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
                Some(d) => Some(PathBuf::from(d).join("rustle.sock")),
                None => crate::cmd::user_config_dir()
                    .ok()
                    .map(|d| d.join("rustle.sock")),
            },
            p => Some(PathBuf::from(p)),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".to_string(),
            workers: 1,
            pin_cores: false,
            admin_listen: "127.0.0.1:8181".to_string(),
            socket_buffer_bytes: 1024 * 1024,
            control_socket: "auto".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub api_keys: Vec<String>,
    /// Client IPs/networks allowed to connect (CIDR; empty = everyone).
    /// Others are closed at accept, on every listener.
    pub allow_clients: Vec<String>,
    /// Wrong keys tolerated per client IP within `failure_window` before a
    /// ban (0 disables). Requests without credentials do not count.
    pub max_failures: u32,
    #[serde(deserialize_with = "de_duration")]
    pub failure_window: Duration,
    #[serde(deserialize_with = "de_duration")]
    pub ban_duration: Duration,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            api_keys: Vec::new(),
            allow_clients: Vec::new(),
            max_failures: 5,
            failure_window: Duration::from_secs(60),
            ban_duration: Duration::from_secs(900),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub max_connections: usize,
    pub max_header_bytes: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: 10_000,
            max_header_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeoutsConfig {
    #[serde(deserialize_with = "de_duration")]
    pub connect: Duration,
    #[serde(deserialize_with = "de_duration")]
    pub idle: Duration,
    /// Total time to receive a request head (from its first byte) or a
    /// SOCKS5 handshake: a client trickling bytes cannot hold a slot.
    #[serde(deserialize_with = "de_duration")]
    pub request_head: Duration,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            idle: Duration::from_secs(60),
            request_head: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    /// Reuse idle upstream connections across requests (profiled: the
    /// per-request connect/close chain dominated the HTTP bulk gap).
    pub upstream_keepalive: bool,
    /// Idle sockets kept per origin (host:port). 0 disables pooling.
    pub max_idle_per_origin: usize,
    /// Max bytes a client request body may reach (Content-Length or
    /// streamed total). 0 = unlimited. On exceed: 413 to the client.
    pub max_request_body_bytes: u64,
    /// Max bytes a response body may relay to the client. 0 = unlimited.
    /// On exceed: relay aborted (the upstream has already been consumed
    /// when known — pool dropped), connection closed.
    pub max_response_body_bytes: u64,
    /// Let `Upgrade: websocket` requests on the proxy's plain-HTTP path
    /// through (the connection becomes a tunnel once the origin answers
    /// 101). `false`: such requests are refused with 403. CONNECT tunnels
    /// (`wss://`, and `ws://` through CONNECT) are opaque and not affected.
    pub allow_websocket: bool,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            upstream_keepalive: true,
            max_idle_per_origin: 256,
            max_request_body_bytes: 0,
            max_response_body_bytes: 0,
            allow_websocket: true,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let raw = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&raw)?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped examples must stay loadable (a duplicated table once
    /// slipped into config.toml.example).
    #[test]
    fn shipped_configs_parse() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for p in [
            "config.toml.example",
            "../bench/dut/rustle/rustle-on.toml",
            "../bench/dut/rustle/rustle-off.toml",
        ] {
            Config::load(&root.join(p)).unwrap_or_else(|e| panic!("{p}: {e}"));
        }
    }
}
