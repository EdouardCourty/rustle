//! The configuration reference (`docs/config.json`), read by the website at
//! the release tag. Keys and defaults come from the serde structs themselves
//! (`Config::default()` serialized in tests), so a key added to the structs
//! without a line in `KEYS` fails the tests, and so does a default that
//! changed without `docs/config.json` being regenerated:
//!
//! ```sh
//! UPDATE_CONFIG_DOCS=1 cargo test -p rustle config_reference
//! ```
//!
//! The same test keeps `docs/config.schema.json` (the JSON Schema of
//! `config.json`, from `config.schema.in.json` with the table names and key
//! paths filled in) current.
//!
//! Whether a key is applied by `rustle reload` or needs a restart is not
//! written here: the test in `server/tests/config_reference.rs` derives it
//! from `restart_required()` and `effective()`.

use std::time::Duration;

use super::Config;

pub(crate) const SCHEMA_VERSION: u32 = 1;

/// Tables, in reading order: (name, description).
pub(crate) const TABLES: &[(&str, &str)] = &[
    (
        "server",
        "Listeners, shards, health endpoint and control socket.",
    ),
    (
        "auth",
        "API keys, client allowlist and bans after wrong keys.",
    ),
    ("limits", "Connection and request-head limits."),
    ("timeouts", "Connect, idle and request-head timeouts."),
    (
        "http",
        "Upstream connection pool, body limits and WebSocket upgrades.",
    ),
    ("dns", "Cache of resolved addresses."),
    ("tls", "HTTPS proxy and HTTP/2."),
    ("filter", "Destination policy."),
    ("socks5", "SOCKS5 listener (TCP CONNECT and UDP ASSOCIATE)."),
    ("log", "Access log."),
];

/// Keys, in reading order: (`table.key`, type, description). Types: `string`,
/// `integer`, `boolean`, `duration`, `string list`, `integer list`,
/// `enum:a|b|c`. Descriptions are plain text with `code` spans only.
pub(crate) const KEYS: &[(&str, &str, &str)] = &[
    ("server.listen", "string", "Plain port (HTTP + CONNECT), literal IP address."),
    ("server.workers", "integer", "Number of shards (one thread each); `0` = one per CPU."),
    ("server.pin_cores", "boolean", "Pin shard i to CPU i and steer each connection to the shard of the CPU that received it (Linux)."),
    ("server.admin_listen", "string", r#"`GET /health`, no auth; `""` disables it."#),
    ("server.control_socket", "string", r#"Unix socket (mode 0600) for `rustle reload`: `/run/rustle/rustle.sock` as root, else `$XDG_RUNTIME_DIR/rustle.sock`, else next to the user config; `""` disables it."#),
    ("server.socket_buffer_bytes", "integer", "`SO_SNDBUF`/`SO_RCVBUF` of relayed sockets; `0` = kernel autotuning. A fixed size (e.g. `1048576`) caps a 100 ms RTT link at ~80 Mbit/s and, on the loopback, can stall bulk flows for seconds."),
    ("auth.api_keys", "string list", "Accepted keys; empty = no authentication (open relay)."),
    ("auth.allow_clients", "string list", "IPs/CIDRs allowed to connect; empty = everyone. Others are closed at accept."),
    ("auth.max_failures", "integer", "Wrong keys tolerated per IP within the window; `0` = no ban. A request without credentials does not count."),
    ("auth.failure_window", "duration", "Counting window."),
    ("auth.ban_duration", "duration", "Ban duration."),
    ("limits.max_connections", "integer", "Concurrent client connections (all shards); beyond: 503."),
    ("limits.max_header_bytes", "integer", "Max request head size (431)."),
    ("timeouts.connect", "duration", "Connection to the origin (504)."),
    ("timeouts.idle", "duration", "Inactivity on a connection or tunnel."),
    ("timeouts.request_head", "duration", "Total time to receive a request head or SOCKS5 handshake."),
    ("http.upstream_keepalive", "boolean", "Reuse connections to origins."),
    ("http.max_idle_per_origin", "integer", "Idle connections kept per origin and per shard (16× that across all origins); a connection idle for over 15 s is closed instead of reused."),
    ("http.max_request_body_bytes", "integer", "Max request body (413); `0` = unlimited."),
    ("http.max_response_body_bytes", "integer", "Max response body (502 when declared, cut otherwise); `0` = unlimited."),
    ("http.allow_websocket", "boolean", r#"Let `Upgrade: websocket` requests on the plain-HTTP path through (the connection becomes a tunnel once the origin answers 101); `false` refuses them with 403. `CONNECT` tunnels (`wss://`) are opaque and not affected: limit them with `[filter] allowed_ports`."#),
    ("dns.ttl", "duration", r#"Lifetime of cached resolutions; `"0s"` disables the cache."#),
    ("dns.max_entries", "integer", "Cache size."),
    ("tls.listen", "string", r#"`"IP:port"` of the HTTPS port, e.g. `"0.0.0.0:8443"`; empty = disabled. Needs `cert` and `key`."#),
    ("tls.cert", "string", "PEM certificate chain."),
    ("tls.key", "string", "PEM private key."),
    ("tls.handshake_timeout", "duration", "Max duration of a TLS handshake."),
    ("tls.http2", "boolean", "Offer HTTP/2 through ALPN."),
    ("tls.ktls", "boolean", "Kernel TLS (Linux, `tls` module). Saves some memory, slower than rustls on throughput without NIC TLS offload."),
    ("filter.block_private", "boolean", "Refuse loopback, private, link-local, CGNAT, multicast and ULA destinations (403). Set `false` to reach your LAN."),
    ("filter.deny", "string list", "Extra refused networks (CIDR)."),
    ("filter.allowed_ports", "integer list", "Allowed destination ports, e.g. `[80, 443]`; empty = all."),
    ("socks5.listen", "string", r#"`"IP:port"` of the SOCKS5 port, e.g. `"127.0.0.1:1080"`; empty = disabled. The password is one of the `api_keys`."#),
    ("log.access", "enum:off|errors|all", "`off`, `errors` (refusals and failures) or `all` (one line per request and tunnel)."),
    ("log.output", "enum:file|stdout", "`file` or `stdout`."),
    ("log.file", "string", r#"Path when `output = "file"`; empty = no access log."#),
    ("log.target", "enum:host|url", "Log `host:port` or the full URL (`url`) of `http://` requests."),
];

/// A JSON value, written by hand to keep serde_json out of the dependencies.
pub(crate) enum Json {
    Str(String),
    Int(i64),
    Bool(bool),
    List(Vec<Json>),
    Obj(Vec<(&'static str, Json)>),
}

impl Json {
    fn s(v: &str) -> Json {
        Json::Str(v.to_string())
    }

    fn write(&self, out: &mut String, depth: usize) {
        let pad = |out: &mut String, n: usize| out.push_str(&"  ".repeat(n));
        match self {
            Json::Str(s) => {
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            Json::Int(n) => out.push_str(&n.to_string()),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::List(v) if v.iter().all(|e| !matches!(e, Json::Obj(_))) => {
                out.push('[');
                for (i, e) in v.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    e.write(out, depth);
                }
                out.push(']');
            }
            Json::List(v) => {
                out.push_str("[\n");
                for (i, e) in v.iter().enumerate() {
                    pad(out, depth + 1);
                    e.write(out, depth + 1);
                    out.push_str(if i + 1 < v.len() { ",\n" } else { "\n" });
                }
                pad(out, depth);
                out.push(']');
            }
            Json::Obj(fields) => {
                out.push_str("{\n");
                for (i, (k, v)) in fields.iter().enumerate() {
                    pad(out, depth + 1);
                    out.push_str(&format!("\"{k}\": "));
                    v.write(out, depth + 1);
                    out.push_str(if i + 1 < fields.len() { ",\n" } else { "\n" });
                }
                pad(out, depth);
                out.push('}');
            }
        }
    }
}

/// `Config::default()` as a TOML table: the keys and their defaults.
fn defaults() -> toml::Table {
    match toml::Value::try_from(Config::default()).expect("Config serializes") {
        toml::Value::Table(t) => t,
        other => panic!("Config serialized to {other:?}"),
    }
}

fn default_of<'a>(table: &'a toml::Table, path: &str) -> &'a toml::Value {
    let (t, k) = path.split_once('.').expect("table.key");
    table
        .get(t)
        .and_then(|v| v.get(k))
        .unwrap_or_else(|| panic!("{path} is not in Config::default()"))
}

fn json_default(v: &toml::Value, ty: &str, path: &str) -> Json {
    match (v, ty) {
        // Duration serializes as { secs, nanos }: written as "1m", "15m", ...
        (toml::Value::Table(t), "duration") => {
            let secs = t["secs"].as_integer().expect("secs") as u64;
            let nanos = t["nanos"].as_integer().expect("nanos") as u32;
            Json::Str(humantime::format_duration(Duration::new(secs, nanos)).to_string())
        }
        (toml::Value::String(s), _) => Json::s(s),
        (toml::Value::Integer(n), "integer") => Json::Int(*n),
        (toml::Value::Boolean(b), "boolean") => Json::Bool(*b),
        (toml::Value::Array(a), "string list" | "integer list") => Json::List(
            a.iter()
                .map(|e| {
                    json_default(
                        e,
                        if ty == "string list" {
                            "string"
                        } else {
                            "integer"
                        },
                        path,
                    )
                })
                .collect(),
        ),
        (v, _) => panic!("{path}: default {v:?} does not fit the declared type {ty:?}"),
    }
}

/// A `Config` with only `path` changed to a value other than its default
/// (what the reload classification feeds to `restart_required`/`effective`).
pub(crate) fn with_changed(path: &str) -> Config {
    let (_, ty, _) = KEYS.iter().find(|k| k.0 == path).expect("known key");
    let def = defaults();
    let cur = default_of(&def, path);
    let value = match (ty.split(':').next().unwrap(), cur) {
        ("boolean", toml::Value::Boolean(b)) => (!b).to_string(),
        ("integer", toml::Value::Integer(n)) => (n + 1).to_string(),
        ("duration", _) => r#""7s""#.to_string(),
        ("enum", toml::Value::String(cur)) => {
            let other = ty["enum:".len()..]
                .split('|')
                .find(|v| v != cur)
                .expect("a second variant");
            format!("{other:?}")
        }
        ("string", _) => r#""changed""#.to_string(),
        ("string list", _) => r#"["changed"]"#.to_string(),
        ("integer list", _) => "[1]".to_string(),
        (t, v) => panic!("{path}: no alternative value for {t} / {v:?}"),
    };
    let (table, key) = path.split_once('.').unwrap();
    toml::from_str(&format!("[{table}]\n{key} = {value}\n"))
        .unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Schema of `docs/config.json` (`docs/config.schema.json`): the template with
/// the table names and key paths of the structs filled in, so that a key
/// added to the structs shows up in the schema.
pub(crate) fn render_schema() -> String {
    let list = |items: Vec<&str>| {
        items
            .iter()
            .map(|i| format!("{i:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    include_str!("config.schema.in.json")
        .replace(
            "\"__TABLE_NAMES__\"",
            &list(TABLES.iter().map(|t| t.0).collect()),
        )
        .replace(
            "\"__KEY_PATHS__\"",
            &list(KEYS.iter().map(|k| k.0).collect()),
        )
}

/// `docs/config.json`; `restart_only` says whether a change to `path` needs a restart.
pub(crate) fn render(restart_only: &dyn Fn(&str) -> bool) -> String {
    let def = defaults();
    let tables = TABLES
        .iter()
        .map(|(name, description)| {
            let keys = KEYS
                .iter()
                .filter(|k| k.0.split_once('.').unwrap().0 == *name)
                .map(|(path, ty, description)| {
                    let mut f = vec![
                        ("key", Json::s(path.split_once('.').unwrap().1)),
                        ("path", Json::s(path)),
                        ("type", Json::s(ty.split(':').next().unwrap())),
                    ];
                    if let Some(values) = ty.strip_prefix("enum:") {
                        f.push((
                            "values",
                            Json::List(values.split('|').map(Json::s).collect()),
                        ));
                    }
                    f.push((
                        "default",
                        json_default(default_of(&def, path), ty.split(':').next().unwrap(), path),
                    ));
                    f.push(("description", Json::s(description)));
                    f.push((
                        "reload",
                        Json::s(if restart_only(path) {
                            "restart"
                        } else {
                            "live"
                        }),
                    ));
                    Json::Obj(f)
                })
                .collect();
            Json::Obj(vec![
                ("name", Json::s(name)),
                ("description", Json::s(description)),
                ("keys", Json::List(keys)),
            ])
        })
        .collect();
    let doc = Json::Obj(vec![
        ("$schema", Json::s("./config.schema.json")),
        ("schema_version", Json::Int(SCHEMA_VERSION as i64)),
        ("version", Json::s(env!("CARGO_PKG_VERSION"))),
        ("tables", Json::List(tables)),
    ]);
    let mut out = String::new();
    doc.write(&mut out, 0);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key of the structs is described exactly once, and every
    /// description is about a key that exists.
    #[test]
    fn every_key_is_described() {
        let def = defaults();
        let mut in_structs: Vec<String> = def
            .iter()
            .flat_map(|(t, v)| {
                v.as_table()
                    .unwrap()
                    .keys()
                    .map(move |k| format!("{t}.{k}"))
            })
            .collect();
        let mut described: Vec<String> = KEYS.iter().map(|k| k.0.to_string()).collect();
        in_structs.sort();
        described.sort();
        assert_eq!(
            in_structs, described,
            "config keys and config::reference::KEYS differ: describe the new key (or drop the old one)"
        );
        let mut tables: Vec<&str> = def.keys().map(String::as_str).collect();
        let mut listed: Vec<&str> = TABLES.iter().map(|t| t.0).collect();
        tables.sort();
        listed.sort();
        assert_eq!(
            tables, listed,
            "config tables and config::reference::TABLES differ"
        );
    }

    #[test]
    fn declared_types_fit_the_defaults() {
        let def = defaults();
        for (path, ty, desc) in KEYS {
            // panics when the default does not fit the declared type
            json_default(default_of(&def, path), ty.split(':').next().unwrap(), path);
            assert!(
                !desc.is_empty() && !desc.contains("**"),
                "{path}: plain description"
            );
        }
    }

    #[test]
    fn enum_variants_are_the_accepted_ones() {
        for (path, ty, _) in KEYS {
            let Some(values) = ty.strip_prefix("enum:") else {
                continue;
            };
            for v in values.split('|') {
                let (t, k) = path.split_once('.').unwrap();
                toml::from_str::<Config>(&format!("[{t}]\n{k} = {v:?}\n"))
                    .unwrap_or_else(|e| panic!("{path} = {v:?} is refused: {e}"));
            }
            let other = with_changed(path); // a second variant exists and parses
            drop(other);
        }
    }
}
