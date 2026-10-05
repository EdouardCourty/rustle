# rustle

A high-performance forward proxy in Rust (Tokio), built for personal use: HTTP/1.1,
HTTPS (CONNECT), HTTPS proxy with HTTP/2, SOCKS5 (TCP + UDP), shared-nothing
multi-core. The original goal: beat the open-source proxies on throughput, first on one
core, then multi-core. Numbers are in [Benchmarks](#benchmarks) and the full history in
[`bench/RESULTS.md`](bench/RESULTS.md).

## Features

**Protocols**

| Client → proxy | |
|---|---|
| Plain HTTP/1.1 (and 1.0) | `http://` requests + `CONNECT` tunnels |
| HTTP/1.1 over TLS ("HTTPS proxy") | credentials and requests encrypted up to the proxy |
| HTTP/2 over TLS (ALPN `h2`) | multiplexed requests and `CONNECT` tunnels |
| SOCKS5 | `CONNECT` + `UDP ASSOCIATE` (QUIC/HTTP3, DNS) |

Through the proxy: anything TCP via `CONNECT` (HTTPS over HTTP/1.1 or HTTP/2, SSH…),
HTTP/3/QUIC via SOCKS5 UDP. Towards `http://` origins the proxy speaks HTTP/1.1
(keep-alive pool).

**Performance** — one shard per core (`SO_REUSEPORT`, `current_thread` runtime,
per-shard upstream pool), allocation-free hot path, zero-copy `splice()` relay for
tunnels and large response bodies, ~2 recv + 2 send per request.

**Security (personal proxy)**
- API key (`Proxy-Authorization: Basic`, or the SOCKS5 password), compared in constant time;
- client IP allowlist (e.g. your VPN range): everyone else is closed at accept;
- temporary ban of IPs after N wrong keys;
- private destinations refused (loopback, RFC 1918, cloud metadata `169.254.169.254`…),
  checked on the resolved address (DNS rebinding covered), plus a `deny` list and
  allowed ports;
- anti-slowloris (total deadline for a request head), anti request smuggling
  (ambiguous `Transfer-Encoding` / `Content-Length` → 400).

**Operations** — access log (file or stdout, never blocking), `GET /health` on an admin
port, graceful shutdown (SIGTERM), log reopen on SIGHUP.

## Install

Linux (x86_64, aarch64; glibc ≥ 2.35: Debian 12, Ubuntu 22.04, RHEL 9 or newer) and macOS
(Apple Silicon, Intel):

```sh
curl -fsSL https://raw.githubusercontent.com/EdouardCourty/rustle/main/install.sh | sh
```

The script detects your system, downloads the release archive, **verifies its SHA-256**
against the `SHA256SUMS` of the same release, and installs `rustle` into `~/.local/bin`
(`/usr/local/bin` as root, or `RUSTLE_INSTALL_DIR`). It never uses `sudo`, and refuses
cleanly on a system whose glibc is too old (or musl/Alpine) instead of installing a
binary that cannot start. Prefer to read it first:

```sh
curl -fsSL https://raw.githubusercontent.com/EdouardCourty/rustle/main/install.sh -o install.sh
less install.sh && sh install.sh
```

Pin a version with `RUSTLE_VERSION=v0.1.0`. Release archives also carry a GitHub build
provenance attestation: `gh attestation verify rustle-<target>.tar.gz --repo EdouardCourty/rustle`.
(macOS: a binary fetched with `curl` is not quarantined, so Gatekeeper does not prompt.)

Then:

```sh
rustle init                # config with a fresh API key (~/.config/rustle/config.toml)
rustle run                 # foreground …
rustle service enable      # … or as a service (systemd user unit / launchd agent)
rustle status              # is it healthy?
rustle update              # latest release (checksum verified)
rustle uninstall           # remove the service and the binary
```

`rustle --help` lists everything; `rustle service enable --system` installs a system-wide
systemd unit (as root: dedicated `rustle` user, `/etc/rustle/config.toml`, `LimitNOFILE=1048576`).
`rustle check [CONFIG]` validates a config without starting the proxy.

## Build from source

```sh
cargo build --release -p rustle
cp rustle/config.toml.example config.toml     # then edit: API key, listen…
./target/release/rustle config.toml           # or: rustle init
```

Quick start, once running:

```sh
# HTTP / HTTPS through the plain port
curl -x http://127.0.0.1:8080 -U me:MY_KEY https://example.com

# HTTPS proxy ([tls] section) — HTTP/1.1, then HTTP/2
curl --proxy https://proxy.example.org:8443 -U me:MY_KEY https://example.com
curl --proxy https://proxy.example.org:8443 --proxy-http2 -U me:MY_KEY https://example.com

# SOCKS5 ([socks5] section)
curl --socks5-hostname me:MY_KEY@127.0.0.1:1080 https://example.com
```

API key: a long random string (`openssl rand -base64 32`). The user name is ignored. On
the plain port and on SOCKS5 the key travels **in clear text**: expose those ports only
on a trusted interface (loopback, VPN); over the Internet, use the HTTPS port.

### Certificate for the HTTPS port

`[tls]` expects a PEM certificate chain and private key. Production: Let's Encrypt on
the proxy's name (`cert = fullchain.pem`, `key = privkey.pem`). Internal use: a
self-signed certificate or an internal CA installed on the clients — browsers refuse an
untrusted HTTPS proxy. Prefer an ECDSA P-256 key (much cheaper handshakes than RSA):

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -days 365 -subj /CN=proxy.example.org -keyout key.pem -out cert.pem
```

The certificate is read at startup: restart after a renewal (graceful shutdown).

## Configuration

A single TOML file (path as argument, default `config.toml`). Commented example:
[`rustle/config.toml.example`](rustle/config.toml.example). Any unknown or misplaced
key fails the startup. Durations look like `"5s"`, `"1m"`, `"15m"`.

### `[server]`

| Key | Default | Meaning |
|---|---|---|
| `listen` | `"0.0.0.0:8080"` | plain port (HTTP + CONNECT), literal IP address |
| `workers` | `1` | number of shards (one thread + one `SO_REUSEPORT` listener each); `0` = one per CPU |
| `pin_cores` | `false` | pins shard *i* to the *i*-th allowed CPU; with one shard per CPU, each connection is steered to the shard of the CPU that received its SYN (`SO_ATTACH_REUSEPORT_CBPF`) |
| `admin_listen` | `"127.0.0.1:8181"` | `GET /health` without auth; `""` disables it |
| `socket_buffer_bytes` | `1048576` | `SO_SNDBUF`/`SO_RCVBUF` of every relayed socket; `0` = kernel autotuning. A fixed size turns autotuning off and is capped by `net.core.rmem_max`/`wmem_max` (warning at startup): at most 1 MB in flight per connection, i.e. ~80 Mbit/s at 100 ms RTT. Use `0` for distant clients |

### `[auth]`

| Key | Default | Meaning |
|---|---|---|
| `api_keys` | `[]` | accepted keys; **empty = no authentication (open relay)** |
| `allow_clients` | `[]` | IPs/CIDRs allowed to connect; empty = everyone |
| `max_failures` | `5` | wrong keys tolerated per IP within the window; `0` = no ban |
| `failure_window` | `"1m"` | counting window |
| `ban_duration` | `"15m"` | ban duration (connections closed at accept) |

A request **without** credentials (a browser's first 407) does not count as a failure.

### `[limits]` and `[timeouts]`

| Key | Default | Meaning |
|---|---|---|
| `limits.max_connections` | `10000` | concurrent client connections (all shards); beyond: 503 |
| `limits.max_header_bytes` | `65536` | max size of a request head (431) or response head |
| `timeouts.connect` | `"5s"` | connection to the origin (504) |
| `timeouts.idle` | `"60s"` | inactivity on a connection or tunnel |
| `timeouts.request_head` | `"10s"` | total time to receive a request head / a SOCKS5 handshake |

### `[http]`

| Key | Default | Meaning |
|---|---|---|
| `upstream_keepalive` | `true` | reuse connections to origins |
| `max_idle_per_origin` | `256` | idle connections kept per origin **and per shard** (global cap 1024 per shard) |
| `max_request_body_bytes` | `0` | max request body size (413); `0` = unlimited |
| `max_response_body_bytes` | `0` | max response size (502 when declared, cut otherwise); `0` = unlimited |

### `[dns]`

| Key | Default | Meaning |
|---|---|---|
| `ttl` | `"60s"` | lifetime of cached resolutions; `"0s"` disables the cache |
| `max_entries` | `4096` | cache size (wiped when exceeded) |

### `[tls]` — HTTPS proxy / HTTP/2

| Key | Default | Meaning |
|---|---|---|
| `listen` | `""` | `"IP:port"` address of the HTTPS port (literal IP): `"0.0.0.0:8443"`; empty = disabled. Requires `cert`/`key` |
| `cert` / `key` | `""` | PEM certificate (chain) and private key |
| `handshake_timeout` | `"10s"` | max duration of a TLS handshake |
| `http2` | `true` | offer HTTP/2 through ALPN |
| `ktls` | `false` | kernel encryption after the handshake (needs the `tls` module, probed at startup) — see [measured decisions](#measured-decisions) |

### `[filter]` — destinations

| Key | Default | Meaning |
|---|---|---|
| `block_private` | `true` | refuses loopback, private networks, link-local, CGNAT, multicast, IPv6 ULA… (403). `false` to reach your LAN |
| `deny` | `[]` | additional refused networks (CIDR) |
| `allowed_ports` | `[]` | allowed destination ports (CONNECT, HTTP, SOCKS5); empty = all. E.g. `[80, 443]` |

### `[socks5]`

| Key | Default | Meaning |
|---|---|---|
| `listen` | `""` | `"IP:port"` address of the SOCKS5 port (literal IP, no host name): `"127.0.0.1:1080"` (local machine / VPN) or `"0.0.0.0:1080"` (all interfaces); empty = disabled. TCP CONNECT + UDP ASSOCIATE. Password = one of the `api_keys`, user name ignored |

### `[log]` — access log

| Key | Default | Meaning |
|---|---|---|
| `access` | `"errors"` | `off` · `errors` (refusals and failures: 403, wrong key, ban, 4xx/5xx, truncated responses) · `all` (+ one line per request and per tunnel) |
| `output` | `"file"` | `file` or `stdout` (journald / `docker logs`) |
| `file` | `""` | file path when `output = "file"`; empty = no access log |
| `target` | `"host"` | `host` (`host:port`) or `url` (full URL of `http://` requests) |

Format: one timestamped `key=value` line per event, for example
`2026-10-01T10:00:00.123Z client=1.2.3.4 via=http1 kind=connect target=example.com:443 status=200 up=773 down=5489 ms=43`.
Writing happens on a dedicated thread and never blocks the proxy (when it falls behind,
lines are dropped and counted: `event=dropped`). Rotation: rename the file, then
`kill -HUP <pid>` (logrotate `postrotate`). Diagnostics (`RUST_LOG`) go to stderr.

## Development

```sh
make check      # fmt + clippy (-D warnings) + tests + quick e2e — before every commit
make test       # unit and integration tests (in process)
make e2e        # black-box functional tests, full suite (~2 min)
make help       # all targets
```

**Functional tests** ([`e2e/`](e2e/README.md)): the real binary, one process per
scenario, driven by real clients (HTTP/1.1, HTTPS, HTTP/2, SOCKS5 TCP/UDP, curl) against
programmable local origins. The suite has ~530 scenarios:
- a matrix of configs × transports × operations × payload sizes, checked byte for byte;
- HTTP and tunnel semantics;
- limits, security (auth, bans, policy), robustness against misbehaving clients;
- signals, access logs.

Every scenario checks that no connection leaks and that SIGTERM shuts down cleanly.
`make e2e F="socks5"` filters by name.

```text
rustle/src/
├── main.rs            startup, signals (SIGTERM/SIGINT stop, SIGHUP log)
├── config/            config types + TOML parsing
├── server/            shards, listeners (plain / TLS / SOCKS5), admin, graceful shutdown
├── proxy/
│   ├── mod.rs         HTTP/1 connection: keep-alive, auth, CONNECT / HTTP routing
│   ├── pipeline.rs    HTTP request: upstream pool, forward, response, chunked uploads
│   ├── h2.rs          HTTP/2: streams, CONNECT on a stream, forward to HTTP/1.1
│   ├── socks5.rs      SOCKS5 CONNECT
│   └── socks5_udp.rs  SOCKS5 UDP ASSOCIATE
├── relay/             transports: zero-copy splice, adaptive copy, chunked
├── http/              head parsing (zero-allocation request, response)
├── stream.rs          generic client transport (TCP / TLS / kTLS)
├── tls.rs, ktls_offload.rs   rustls, kernel offload
├── policy.rs          destinations, client allowlist, bans
├── accesslog.rs       access log
├── auth.rs, dns.rs, upstream.rs
e2e/                   black-box functional tests (see e2e/README.md)
bench/                 comparison harness (see below)
```

Documentation for agents / contributors: [`AGENTS.md`](AGENTS.md).

## Benchmarks

Docker harness: in-house load generator (`bench/generator`), nginx origin, the proxy
under test pinned to **one core**, the harness on the other, test order shuffled with a
fixed seed (`2004113323`), median of 3 runs. Compared proxies: 3proxy, Squid, gost,
Tinyproxy (same key, access logs off, same certificate type).

```sh
make bench P=rustle A=on                 # one matrix (P = rustle|3proxy|squid|gost|tinyproxy)
make bench-all                            # full campaign: 5 proxies × plain / TLS / h2, then tables
PROXY_TLS=1 ./bench/run-bench.sh squid on # one transport (PROXY_H2=1 for HTTP/2)
NCPUS="1 2 4 8" make bench-scaling        # multi-core curve (machine with ≥ 2 × N cores)
```

### Latest numbers (2026-10-03 campaign, one core, auth on)

Throughput · proxy CPU per request. No proxy-side error in any test. Measurement
protocol M21.5 (warmup outside the sample): do not compare with earlier campaigns.

**Plain (HTTP/1.1)**

| Test | **rustle** | 3proxy | Squid | gost | Tinyproxy |
|---|---|---|---|---|---|
| 1 KB keep-alive | **14,594 rps · 44 µs** | 1,557¹ · 67 µs | 8,964 · 110 µs | 1,935 · 510 µs | 2,962 · 313 µs |
| 10 KB | **14,211 · 54 µs** | 12,343 · 74 µs | 7,292 · 136 µs | 1,850 · 521 µs | 2,601 · 353 µs |
| 1 MB | **2,453 · 224 µs** | 2,159 · 329 µs | 306 | 688 | 128 |
| 10 MB | **319** | 264 | 31 | 104 | 14 |
| Short connections | **4,884 · 75 µs** | 3,403 · 230 µs | 3,795 · 212 µs | 1,916 · 507 µs | 2,996 · 306 µs |
| CONNECT tunnels (flood) | **19.3 Gbps** | 18.4 | 13.0 | 11.6 | 1.2 |
| 25,000 tunnels (peak RAM) | **25,000 · 78 MB** | 17,154 · 380 MB | 25,000 · 896 MB | —² | 16,746 · 314 MB |

**HTTPS proxy (TLS)**

| Test | **rustle** | Squid | gost |
|---|---|---|---|
| 1 KB | **14,128 · 50 µs** | 7,826 · 126 µs | 1,611 · 601 µs |
| 1 MB | **1,444 · 654 µs** | 178 | 468 |
| Short connections (handshake) | **2,727 · 233 µs** | 1,600 · 611 µs | 1,566 · 605 µs |
| CONNECT tunnels (flood) | **11.4 Gbps** | 4.8 | 6.7 |
| 25,000 tunnels (peak RAM) | **25,000 · 788 MB** | 25,000 · 1,846 MB | 1,226² |

**HTTP/2**

| Test | **rustle** | gost |
|---|---|---|
| 1 KB | **14,896 · 57 µs** | 3,904 · 254 µs |
| 1 MB | **792 · 1,190 µs** | 317 · 3,093 µs |
| CONNECT tunnels (flood)³ | 7.4 Gbps · **0.91 s CPU/GB** | 7.6 Gbps · 1.05 s CPU/GB |
| 25,000 tunnels⁴ | **21,264 · 941 MB** | 462 |

¹ 3proxy at 1 KB: held back by its 40 ms delayed ACK (known artifact, see RESULTS.md).
² gost exceeds the machine's RAM (4 GB): container killed or tunnels refused during the test.
³ Throughput bound by the harness core (generator + sink): the throughput gap (−3 %) is
within flood noise (±15 %); at equal throughput rustle spends 13 % less CPU per GB.
⁴ Harness ceiling (the generator's TLS handshakes on the shared core, p50 1.3 s), stable
since the first campaign.
rustle is first on every row except HTTP/2 flood throughput (a tie within noise, better
CPU per GB).
Full tables: [`bench/RESULTS.md`](bench/RESULTS.md) (M21 section, final campaign).

## Measured decisions

Every performance choice went through the fixed-seed matrix (details in
`bench/RESULTS.md`):
- **one Tokio `current_thread` runtime per shard**: −11 % CPU per request on short
  connections compared with a one-worker multi-thread runtime;
- **no io_uring**: a monoio prototype (M18, removed from the repo, commit `a2b11e3`)
  measured ±8 % depending on size, no net gain — on one core the cost is in the kernel
  TCP stack (~80 % of the CPU); to be retested multi-core;
- **kTLS as an option**: +13 % on CONNECT tunnels and −30 % memory at 25,000 tunnels,
  but −37 % on large `http://` GETs and +15 % CPU per new connection;
- **batched access log**: +2.9 % CPU per request in `all` mode (waking the writer thread
  once per line cost +26 %);
- **allocator**: mimalloc tested, neutral (the hot path does not allocate);
- **fixed 1 MB socket buffers** rather than autotuning on the bench (LAN): flood −56 % CPU
  per GB, 10 MB +30 % throughput, 1 KB −10 % CPU; autotuning stays available for WAN use;
- **`TCP_DEFER_ACCEPT`**: −7 % CPU per short connection (the `accept` wakeup waits for
  the request); QUICKACK once per connection: neutral, kept;
- **per-shard pool of splice pipes**, kernel-shrunk pipes refused: beyond ~60 tunnels,
  the `fs.pipe-user-pages-soft` budget handed out 8 KB pipes (flood at 128 tunnels
  10.0 → 16.9 Gbps).
