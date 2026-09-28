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
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub limits: LimitsConfig,
    pub timeouts: TimeoutsConfig,
    pub dns: DnsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
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
#[serde(default)]
pub struct ServerConfig {
    pub listen: String,
    pub workers: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".to_string(),
            workers: 1,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AuthConfig {
    pub api_keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
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
#[serde(default)]
pub struct TimeoutsConfig {
    #[serde(deserialize_with = "de_duration")]
    pub connect: Duration,
    #[serde(deserialize_with = "de_duration")]
    pub idle: Duration,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            idle: Duration::from_secs(60),
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
