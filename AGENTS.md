# AGENTS.md — rust-proxy

Forward HTTP/HTTPS (CONNECT) proxy in Rust, Tokio architecture.
**Goal: beat every open-source proxy on throughput, first on 1 core, then multi-core.** Current reference: 3proxy.

Platforms: **Linux (reference, all fast paths) and macOS (dev/test)**. Off Linux: no splice (`relay/splice_stub.rs`: adaptive copy only), no kTLS, no `TCP_QUICKACK`/`DEFER_ACCEPT`, `pin_cores` ignored, shards share one cloned listener (macOS `SO_REUSEPORT` does not balance). Bench = Docker/Linux only. e2e on macOS uses `lsof`/`ps` instead of `/proc`; UDP matrix capped at 8 KB.

Language: **everything in English** — code, comments, commit messages, README, RESULTS.md, all docs.

## Layout

```
Cargo.toml            workspace (rustle + bench/generator + e2e), [profile.release] lto=fat, codegen-units=1; [profile.e2e] (release without LTO) for the e2e harness
Makefile              help build test e2e e2e-quick fmt check clippy images up down smoke bench bench-all bench-scaling charts latest clean
rustle/              the proxy
  config.toml.example
  src/
    main.rs           dispatch (cli.rs: run | init | check | status | service | update | uninstall), `run`: tracing (RUST_LOG, on stderr, ANSI only on a TTY), config = path or lookup, RLIMIT_NOFILE raised, signal handlers installed, then server::start, then wait for SIGTERM/SIGINT (SIGHUP = log reopen)
    config/mod.rs     TOML → structs (humantime for durations)
    server/mod.rs     shards (thread + current_thread runtime + SO_REUSEPORT plain + TLS listeners + pool), TLS handshake then h2/h1 dispatch by ALPN, global max_connections counter, admin /health, trim, 5 s graceful shutdown
      server/live.rs     `Live` snapshot (config, auth, DNS, policies, limiter, TLS acceptor), `restart_required`/`effective`, reload
      server/socket.rs   listener binding (SO_REUSEPORT, backlog, DEFER_ACCEPT), socket buffers, control socket bind
      server/cpu.rs      core pinning, SO_ATTACH_REUSEPORT_CBPF steering
      server/control.rs  admin `/health` listener and control socket (`reload`)
      server/tests/      in-process end-to-end tests, one file per feature (helpers in tests/mod.rs)
    stream.rs         ClientStream trait (TcpStream | TlsStream): tcp()/into_tcp() = splice/QUICKACK paths, TLS = user-space copy; ReadAhead under rustls (which reads 4 KB at a time): adaptive 64 KB buffer, only for sustained flows
    tls.rs            rustls (ring): PEM cert/key, ALPN h2 + http/1.1, AES-128-GCM first + server order, stateless tickets, secret extraction when kTLS
    ktls_offload.rs   kTLS: startup probe (TCP_ULP), rustls drain (CorkStream), keys → SOL_TLS; KtlsClient (already-decrypted bytes first, EIO = control record = end, close_notify via sendmsg); tcp()/into_tcp() once nothing is buffered → splice on the TLS port (tunnel half-close sends close_notify first)
    proxy/mod.rs      handle_conn<C: ClientStream>: serve() keep-alive loop (read_head → auth → classify → CONNECT | HTTP), tunnel set up afterwards
    proxy/socks5.rs   SOCKS5 (RFC 1928/1929): CONNECT (splice relay) + UDP ASSOCIATE, password = API key, BIND refused
    proxy/socks5_udp.rs UDP ASSOCIATE (QUIC/HTTP3, DNS): client-side socket + dual-stack outbound socket; datagrams accepted only from the TCP control connection's IP, replies only from contacted destinations, same policy as TCP; lives as long as the TCP control connection
    proxy/h2.rs       HTTP/2 (ALPN h2): 1 task/stream; CONNECT = h2 ↔ TCP tunnel (flow control); other methods → HTTP/1.1 through the pool, chunked decoded
    proxy/upgrade.rs  WebSocket handshake on the plain-HTTP path → tunnel (see Architecture)
    proxy/pipeline.rs http(): forward_request, retry, 1xx, body framing, upstream pooling
    http/request.rs   RequestHead = Spans into the buffer (zero-alloc parse), classify, parse_authority
    http/response.rs  response head parse, keep_alive, framing
    relay/mod.rs      bidirectional (splice on Linux), IdleTimer (inactivity timeout)
    relay/adaptive.rs adaptive copy 8→64 KB, then switch to splice for the rest of the body; immediate splice when the declared remaining body is ≥ 128 KB
    relay/splice.rs   pipe2 + splice via TcpStream::try_io, 1 MB PipePair (per-shard pool, 16 max, returned when empty; pipes < 64 KB refused → degraded_pipes), lazy tunnel() (adaptive copy, then pipe when sustained), dir() (fill-then-drain; a read error ends the direction like EOF)
    relay/chunked.rs  transparent chunked relay (verbatim framing), both ways: upstream→client responses and client→upstream uploads; over budget = FileTooLarge, bad framing = InvalidData
    auth.rs           Basic auth, password ∈ api_keys (ct_eq over all keys, no short-circuit), base64 on the stack
    cmd/reload.rs     `rustle reload`: sends `reload` over the control socket, prints the answer
    accesslog.rs      access log: off/errors/all levels, timestamped key=value lines, bounded try_send channel (never blocking, drops counted), writer thread (200 ms flush, accept refusals aggregated 1/s, SIGHUP reopen)
    policy.rs         DestPolicy (allowed ports before DNS, non-public ranges + deny CIDRs on the resolved address); ClientPolicy (client IP allowlist, at accept); AuthLimiter (per-IP bans after N wrong keys, lock-free while nobody fails)
    dns.rs            positive cache, TTL (outside the lock), Mutex<HashMap>
    upstream.rs       per-origin keep-alive pool (LIFO)
docs/benchmarks/      SVG charts embedded in the README (generated, committed)
install.sh            curl|sh installer (POSIX sh): detects target, glibc gate, SHA-256 check, no sudo; tested by `.github/scripts/test-installer.sh` (CI `installer` job)
.github/workflows/    ci.yml (lint, tests+e2e on Linux/macOS, installer), release.yml (tag vX.Y.Z → 4 targets → SHA256SUMS + VERSION + attestation → smoke install)
e2e/                  black-box functional tests (see e2e/README.md): the release binary started per scenario (generated config, ports read from the `rustle listening` line), programmable local origins/targets, HTTP/1.1, TLS, h2, SOCKS5, curl clients
  src/harness/        proxy.rs (Config, Proxy: spawn, /proc, signals, finish = CLOSE_WAIT leak check + clean SIGTERM), origin.rs, targets.rs, payload.rs, certs.rs
  src/clients/        http1.rs, h2.rs, socks5.rs, curl.rs
  src/suites/         one file per feature: matrix http connect h2 socks5 uploads limits security robustness process logs interop_curl
bench/
  run-bench.sh <proxy> <on|off>            matrix (proxy = rustle|3proxy|squid|gost|tinyproxy)
  run-campaign.sh                          full campaign (5 proxies × plain/TLS/h2) → results/campaign-<ts>.txt; tables: tools/campaign-table.py; `MATRIX=ws` → campaign-ws-<ts>.txt
  run-scaling.sh                           multi-core curve (PROXIES, NCPUS, AUTH, PROXY_TLS, PROXY_H2), `REMOTE=1` = three machines
  env.sh, docker-compose.yml               cpuset: proxy on PROXY_CPU, everything else on BENCH_CPU
  generator/                               bench-client crate (http | flood | hold | ws | sink | upstream): shared-nothing threads (one current_thread runtime each), prebuilt requests, per-thread histograms; `upstream` = static Rust origin (replaces nginx, ~3 µs/req vs ~9.5), `sink` = per-core SO_REUSEPORT discard
  dut/<proxy>/                             Dockerfiles + on/off configs (user bench / key secret1, port 8080); `# bench-ports: tls=<p> h2=<p>` line = supported modes (self-signed ECDSA P-256 cert generated at build); WORKERS read by the entrypoint (rustle, squid); rustle/Dockerfile.debug = strace; the rustle Dockerfile stubs the e2e workspace member
  upstream/                                nginx (1kb/10kb/1mb/10mb objects)
  tools/local-upstream-http11.py           local HTTP/1.1 keep-alive upstream (up11.py)
  docker-compose.seccomp.yml               `SECCOMP=unconfined` overlay (io_uring; apply it to every compared DUT)
  matrix.sh                                workload list + seeded shuffle, sourced by run-bench.sh and run-remote.sh; `MATRIX=ws` = WebSocket workloads
  run-remote.sh <proxy> <on|off>           same matrix on three machines (generator / proxy / origin+sink over SSH, host network); see REMOTE.md. run-scaling.sh REMOTE=1 = scaling curve on it
  FAIRNESS.md                              competitor versions, settings and the A/B behind each one (read before touching dut/*)
  RUNBOOK.md                               the final-benchmark procedure, step by step (phase A: three machines, phase B: tunnels on the Mac); start here
  preflight.sh, remote/                    bootstrap.sh (prepare a machine), probe.sh + preflight.sh (compare the three machines, measure links, results/preflight-<ts>.txt), apply-proxy-host.sh (APPLY=1: NIC interrupts and hyperthread siblings off the proxy cores)
  REMOTE.md                                three-machine setup: sizing, sysctls, validity checks (`gen_saturated`)
  tools/charts.py                          SVG charts for the README from a campaign index → docs/benchmarks/ (`make charts`)
  RESULTS.md                               published summary and measurement history
  results/                                 raw outputs (gitignored)
```

## Architecture (what you need to know)

- **Threading**: shared-nothing. `workers` = N shards (0 = one per CPU): each one OS thread, one `current_thread` runtime, one `SO_REUSEPORT` listener on the same port (the kernel spreads connections), its own upstream pool. A connection stays on its shard. Shared: `Config`, `Authenticator`, `DnsCache` (Mutex, cold), atomic counters (`active` = global max_connections, `peak`, per-shard `accepted` logged at shutdown). Optional `pin_cores` (one shard per CPU → `SO_ATTACH_REUSEPORT_CBPF` steering: connection to the shard of the CPU that received the SYN, hash otherwise). Shard 0: admin + `malloc_trim` after bursts. NODELAY + `TCP_DEFER_ACCEPT` + buffers (`socket_buffer_bytes`) set on the listener (inherited). Per-shard `accepted` on its own cache line; `ConnSlot` holds the shard's `Arc<Services>` (no shared refcount). Main waits for the signal, then `shutdown_and_join`.
- **Client transports**: plain port (HTTP/1.1, splice) and optional TLS port (`[tls]`): HTTP/1.1 through the same generic pipeline (`ClientStream`, monomorphized: the plain path compiles identically) or HTTP/2 when ALPN negotiates `h2`. Over TLS: no splice (the socket carries records), `flush()` after every write (rustls may keep ciphertext buffered), `close_notify` at end of connection. Upstream side: always plain TCP HTTP/1.1.
- **Policy**: `connect_upstream` refuses (403 / SOCKS REP 0x02) a port outside the allowlist, then any resolved address that is non-public or in `deny` (protects against DNS rebinding); applied to CONNECT, HTTP, h2 and SOCKS5. An IP outside `allow_clients` or banned (wrong keys) is closed at accept, before any read. One connection = one `Arc<Services>` (config, auth, DNS, policy, limiter, shard pool).
- **CONNECT**: `host:port` key → DNS → connect → `200` → early bytes → `relay::bidirectional` (splice::tunnel: per direction, 8→64 KB copy then a pooled PipePair when sustained; half-close via `shutdown(SHUT_WR)`, preceded by close_notify on a kTLS client). `serve`'s keep-alive state is boxed: a held tunnel's future keeps only the relay state (test `connection_future_stays_small`).
- **HTTP**: origin key → `pool.take` or connect → `forward_request` (head rewritten into `rewrite_buf`, one `write_all` syscall without body, `write_vectored` head+prefix otherwise) → `read_response_head` (httparse, 64 headers) → body by framing (bounded CL / chunked / EOF) → back to the pool when complete + keep-alive. An EOF-delimited body closes the client connection after the response.
- **Reload** (`rustle reload` → control socket, or SIGHUP; both = `server::reload_from_file`): re-reads the file, builds a new immutable `Live` snapshot (config, auth, DNS cache, policies, limiter, TLS acceptor; all validated first, error = nothing changes) and publishes it on a `watch` channel. Each shard checks `has_changed()` after `accept` (one atomic load) and rebuilds its `Arc<Services>`; its pool survives. Open connections keep the snapshot they were accepted under. Restart-only keys (`restart_required()`: listeners, workers, pin_cores, socket buffers, pool, `[log]`, `tls.listen/ktls`, `socks5.listen`, `admin_listen`) keep their running value (`effective()`) and are reported. The DNS cache and the ban list are kept when their settings did not change. Any new config key must be classified: reloadable (read through `svc.cfg` / `Live`) or added to `restart_required` + `effective`. Tests: `server/tests/reload.rs`, e2e `reload/*`.
- **WebSocket** (`proxy/upgrade.rs`): `GET` + `Connection: Upgrade` + `Upgrade: websocket` (`RequestHead::upgrade`) skips the keep-alive pipeline: fresh upstream connection (never pooled), head rewritten with `Connection: Upgrade`, `101` → head + bytes already read forwarded, then the same `Tunnel` as CONNECT (splice; access log kind `websocket`, status 101). Not 101 → the origin's answer is relayed and the connection closes. `http.allow_websocket = false` → 403 before any upstream connect. Policy, auth and limits apply as for any request. h2 clients: no RFC 8441 extended CONNECT.
- **Errors before any response byte**: no response / connection closed by the origin → 502; idle timeout on the response head → 504 (h2: same statuses on the stream). Upstream write failure → retry when replayable, 502 otherwise.
- **Retry** on a stale pooled socket: GET/HEAD with a fully buffered body only (HTTP/1 and h2).
- **Uploads**: `Transfer-Encoding: chunked` accepted (alone, without Content-Length, not in HTTP/1.0 → 400 otherwise) and relayed verbatim (staging = `rewrite_buf`), budget enforced while relaying (→ 413), bad framing → 400, bytes after the final chunk → close after the response; not replayable. A Content-Length body cut by the client aborts both sides at once. In h2, a body without Content-Length goes upstream as chunked.
- **Pipelined client requests**: the first one is answered, then the connection closes (the client retries the rest, RFC 9112 §9.3.2).
- **Limits**: `max_request_body_bytes` → 413, `max_response_body_bytes` → 502 (declared CL) or cut (unknown).
- **Measured syscalls**: ~2 recv + 2 send / request, epoll_ctl ≈ 0. NODELAY 1×/socket, QUICKACK 1×/connection, SO_SNDBUF/RCVBUF 1 MB, 1 MB pipes.

## Config (TOML, the only input for the proxy itself; reloadable at runtime, see Reload)

CLI (clap derive: `rustle/src/cli.rs` = `Cli` + `Command`; one file per command in `rustle/src/cmd/` with its own `Args` + `run`): `rustle [run] [CONFIG]` (a bare path = config, kept for Docker/e2e/bench), `init`, `check`, `status`, `reload`, `service <install|uninstall|enable|disable|start|stop|restart|status|logs> [--system] [--dry-run]`, `update [--check]`, `uninstall`, `--version`. No flags change proxy behavior: everything is in the TOML. Default config lookup: `./config.toml`, `~/.config/rustle/config.toml`, `/etc/rustle/config.toml`.

| Key | Default |
|---|---|
| `server.listen` | `0.0.0.0:8080` (literal SocketAddr) |
| `server.workers` / `pin_cores` | 1 (0 = one shard per CPU) / false |
| upstream pool | limits are **per shard** |
| `server.admin_listen` | `127.0.0.1:8181` (`""` = off, `GET /health` only) |
| `server.control_socket` | `"auto"` (`/run/rustle/rustle.sock` as root, else `$XDG_RUNTIME_DIR/rustle.sock`, else next to the user config; `""` = off); Unix socket, mode 0600, one line command → one line answer (`ok …` / `error …`), only `reload` today: the place for future control commands |
| `server.socket_buffer_bytes` | 1048576 (0 = kernel autotuning; fixed size = autotuning off, capped by `rmem_max`/`wmem_max`, warning at startup; 1 MB wins on the LAN bench, 0 advised for distant clients) |
| `auth.api_keys` | `[]` → **auth off = open relay** |
| `limits.max_connections` / `max_header_bytes` | 10000 / 65536 |
| `timeouts.connect` / `idle` | 5s / 60s |
| `dns.ttl` / `max_entries` | 60s (0 = off) / 4096 |
| `http.upstream_keepalive` / `max_idle_per_origin` | true / 1024 (global cap = 16× per shard; a socket idle > 15 s is dropped, a full origin evicts its oldest socket) |
| `http.max_request_body_bytes` / `max_response_body_bytes` | 0 = unlimited |
| `http.allow_websocket` | true (`Upgrade: websocket` on the plain-HTTP path; false → 403; CONNECT/wss untouched) |
| `tls.listen` / `cert` / `key` | `""` (off) / PEM |
| `tls.handshake_timeout` / `http2` / `ktls` | 10s / true (ALPN h2) / **false** (opt-in: +13 % tunnels, −30 % RAM, but −37 % HTTP bulk) |
| `auth.allow_clients` | [] (empty = everyone; CIDR, closed at accept on every listener) |
| `auth.max_failures` / `failure_window` / `ban_duration` | 5 / 1m / 15m (0 = off; missing credentials do not count) |
| `timeouts.request_head` | 10s (total head / SOCKS5 handshake, anti-slowloris) |
| `filter.block_private` / `deny` / `allowed_ports` | **true** / [] / [] (empty = all) — bench configs set false (private Docker network) |
| `socks5.listen` | `""` (off); `"IP:port"`, literal IP |
| `log.access` / `output` / `file` / `target` | errors / file (or stdout) / `""` (off) / host (or url) — bench: off, `ACCESS_LOG=all` to measure |

`deny_unknown_fields` on every section: an unknown or misplaced key fails the load.

## Commands

```bash
cargo build --release -p rustle         # release profile, no target-cpu (the bench image builds the same binary)
cargo test -p rustle                    # inline tests (#[cfg(test)]), including in-process end-to-end ones in server/tests/
make check                               # fmt + clippy -D warnings + tests + e2e --quick (~20 s) — before every commit
make e2e [F="filter"]                    # full e2e suite (~2 min, ~530 scenarios) — before every release or large change
cargo run --profile e2e -p e2e -- --list # scenarios; --keep keeps the directories (config, logs)
make bench P=rustle A=off R=3           # full matrix (see below)
BENCH_ONLY=<test> BENCH_SEED=2004113323 bench/run-bench.sh rustle off
```

Optional features: `jemalloc`, `mimalloc` (mutually exclusive; default = glibc malloc).

## Benchmarks — rules

- **Protocol**: warmup in a separate run (`--warmup-only`) before sampling → numbers ~6–8 % lower than campaigns run before this protocol, do not compare with them. Per-run netns TCP counters: `segs_out_per_run`, `active_opens_per_run` (pool churn), `retrans_per_run`, `delayed_acks_per_run`, `listen_overflows`, `listen_drops`. `flood-128` outside the matrix (`BENCH_ONLY`): beyond the kernel budget for 1 MB pipes.
- A/B: alternate base/variant (2–3 passes), never a variant alone; change nothing in the repo during an A/B (the image is rebuilt on every run).
- Reference seed **`2004113323`**. Shuffled test order (LCG), proxy restarted before each test. Noise ±3–5 %: below that, no conclusion.
- Tests: http-1kb (64 conns), http-10kb (16), http-1mb (16), http-10mb (8), http-1kb-short (no keep-alive), flood-8, hold 1k/5k/10k/25k.
- Output: `RESULT median ...` lines (rps, mbps, p50, errors, `err_kinds`, `reconnects`, proxy/generator/upstream CPU, `rss_mb` = residual after the test, `peak_rss_mb` = VmHWM = real cost). Proxy CPU = the container's cgroup, RSS = sum over processes (multi-process DUTs such as Squid SMP included).
- Transports: `PROXY_TLS=1` (HTTPS proxy) / `PROXY_H2=1` (HTTP/2 over TLS) → results under `results/<proxy>-tls|-h2/`. TLS: rustle, Squid, gost. h2: rustle, gost (3proxy/Tinyproxy: plain only). Generator: `--proxy-tls`, `--proxy-h2` (1 h2 connection per conn, sequential requests; CONNECT = 1 stream on its own connection), certificate not verified but handshake signatures verified.
- `KTLS=true|false` (rustle, TLS port), `SECCOMP=unconfined` (io_uring).
- Multi-core: `PROXY_NCPU=<n>` (first n CPUs to the proxy, the rest to the harness) or explicit `PROXY_CPU`/`BENCH_CPU`; `WORKERS=<n>`; `GEN_THREADS` = generator threads (size them on BENCH_CPU). The results.txt header = the run's parameters.
- Harness threads: `GEN_THREADS`, `UP_THREADS` (origin), `SINK_THREADS` (compose reads them). Measured on 11 CPUs (proxy 0..n-1, harness 8-10): old harness (nginx + multi-thread generator) = 15.5 µs/req at 1 KB; new = 7 µs/req → rustle 1 KB 264k → 330k rps (2 cores), 251k → 358k (4 cores). flood needs `GEN_THREADS=1 SINK_THREADS=2` on 3 harness cores (109 Gbps at 4 proxy cores; 1 sink thread caps ~75).
- Generator: honors `Connection: close`/HTTP/1.0 (reconnect) and replays a GET once on a reused connection closed before any byte (RFC 9112 §9.3.1) → `reconnects` (Tinyproxy: 1 per request, it closes without announcing it).
- Metric to track: **proxy CPU per request** (`cpu_s_per_run / (rps × duration)`), throughput being capped by the harness. Noise between two identical matrices: up to ±7 % CPU, ±15 % on flood.
- Measured split (1kb): user ≈ 9 µs/req, sys ≈ 35 µs/req. Syscalls at the floor. Profiling: `dut/rustle/Dockerfile.debug` image (strace). No perf on the box (paranoid=4).
- `ALLOCATOR="--features mimalloc" ./run-bench.sh …` for an allocator A/B (retested: neutral). **`err_kinds` must be empty.**
- Do not build images in parallel (OOM on 4 GB). The harness shares 2 cores: ceiling ~23 µs/req on the generator+nginx side.
- **TLS parity**: the generator offers AES-128-GCM first and keeps an unbounded ticket store; every TLS RESULT carries `tls_negotiated` and `tls_resumed`; the campaign runs the short TLS test with (`tls`) and without (`tlsfull`, `TLS_RESUME=0`) resumption; `tools/validity.py` flags differing ciphers and unexpected resumption.
- **Sweep + saturation**: every load test runs at several `--conns` levels (`sweep_levels` in matrix.sh); `campaign-table.py` / `charts.py` keep the best clean level and show `@conns`. `proxy_util`, `origin_util`, `sink_util` and the `proxy_unsaturated` / `*_saturated` flags say whether the harness reached the limit: a best level flagged unsaturated is not a result. Measured: 3proxy needs ~1000 connections to fill a core, rustle collapsed above ~500 until the pool caps were raised.
- **Run rules** (see `bench/FAIRNESS.md`): `run-remote.sh` needs `PROXY_CPUS` and a matching `WORKERS`; `check_valid` (matrix.sh) marks a run INVALID (exit 3) on any `err_kinds` or a hold with fewer tunnels than asked; the generator validates body sizes. The `auth=off` rustle config really has no key.
- **Fairness (non-negotiable)**: every competitor on its latest stable release with the tuning its own docs recommend, each choice A/B-tested and recorded in `bench/FAIRNESS.md` (3proxy needs `-osTCP_NODELAY -ocTCP_NODELAY`: without it the old "×9.5 at 1 KB" was a Nagle + delayed-ACK artifact, not 3proxy's speed). Never publish a number against a default-config competitor; when a version or setting changes, update FAIRNESS.md and re-run the whole campaign. rustle's bench binary is the shipped release build (no `target-cpu=native`); `SOCK_BUF=0` runs it on kernel autotuning like the competitors that have no buffer option.
- 1 KB is limited by harness latency (~15k rps); the proxy alone is estimated at 21–24k rps (~42–47 µs/req).
- Do not compare numbers across bench protocol changes.
- Multi-core: not measured with the single-host harness (2 cores). Use the three-machine setup (`bench/REMOTE.md`, `run-remote.sh`): a result is valid only if the generator is not saturated.
- Never compile or edit `run-bench.sh` during a bench (shared CPU; bash rereads the running script).

### Benchmark tooling

Pipeline, one command: `make bench-publish` (`bench/run-all.sh`) = `run-campaign.sh` (index `bench/results/campaign-<ts>.txt`; `REMOTE=1` for the three-machine setup) + `MATRIX=ws` campaign → `results/report-<ts>.md` (`tools/campaign-table.py`) → `tools/validity.py` (invalid, unsaturated, missing or pre-sweep points; exit 1) → `tools/charts.py` (committed SVGs in `docs/benchmarks/`, parsing in `tools/benchlib.py`, `myproxy` mapped to `rustle`) → `tools/readme-table.py --write` (the README table between its `bench:table` markers). `REPORT_ONLY=1` rebuilds artifacts from the newest indexes without running anything. After a campaign: review the validity output, commit the SVGs and README, then update `bench/RESULTS.md` and Known state.

WebSocket matrix (`MATRIX=ws`, `make bench-ws P=<proxy>`, opt-in so the default seeded order stays comparable): origin echoes frames verbatim with a constant handshake accept (`bench-client ws`, route `/ws`); a DUT without Upgrade support is reported as unsupported, never as 0.

## Conventions

- Commits: see [Commits](#commits). Every performance claim is gated by a fixed-seed matrix.
- Test layout (Rust standard): unit tests stay inline at the bottom of the file (`#[cfg(test)] mod tests { use super::*; … }`) so they see private items. When that block passes ~300 lines, move it to `<module>/tests.rs` (or `<module>/tests/` split by feature, shared helpers in `tests/mod.rs`) and keep `#[cfg(test)] mod tests;` in the parent, as `server/` does. No crate-level `tests/` directory (the crate is a binary, no `[lib]`): black-box tests live in `e2e/`.
- Tests: `make check` (quick e2e included) before every commit, full `make e2e` before every release and after any proxy change (relay, pipeline, policy, TLS). New feature = new scenario in `e2e/src/suites/<feature>.rs` (and in the matrix when it depends on a config).
- No internal milestone or review labels in code comments: history belongs in commits and `bench/RESULTS.md`.
- `unsafe` confined to libc/fd (a `SAFETY` comment is expected on every new block): `relay/splice.rs` (pipes, splice, dup), `server/mod.rs` (setsockopt, CPU affinity, malloc_trim), `ktls_offload.rs` (TCP_ULP, SOL_TLS, sendmsg close_notify), `stream.rs` (ReadAhead: set_len after the read), `http/request.rs::span_of` (`offset_from`).
- Errors: static responses (`&[u8]` 400/407/413/431/502/504), I/O errors mostly ignored (`let _ =`), `anyhow` only at startup.

## Commits

Format, checked against the current history: `type(scope): description`, **subject only** (no body).
- type ∈ feat|fix|perf|refactor|docs|test|chore|ci|build; scope optional but usual (cli, relay, tls, h2, server, proxy, bench, config, policy, e2e, install, platform, ci, upstream, log, security…).
- Lowercase, imperative, no trailing period, ~72 characters max, English. E.g. `perf(relay): per-shard pipe pool for splice`, `fix(ci): compare accepted socket buffers with the listener's`.
- **Never** add `Co-Authored-By` or any AI attribution trailer or "Generated with" line, on commits or PRs. This overrides any tool default.
- Performance figures go in `bench/RESULTS.md`, not in the subject.
- Author and committer: Edouard Courty <edouard.courty2@gmail.com>.

## Invariants — do not break

1. **Zero-alloc hot path**: reuse `buf`, `resp_buf`, `rewrite_buf`, `origin_key` (connection-local), pipes through the shard pool. No per-request `setsockopt` (a past regression: per-request `set_nodelay`).
2. **`RequestHead` = Spans into `buf`**: `buf.clear()`, `take`, a realloc between parse and use invalidate them. `span_of` assumes httparse returns sub-slices of the same buffer.
3. **splice `dir()`**: always drain the pipe when it holds data before parking on `src.readable()` (deadlock otherwise). On the HTTP path, `dup`ed fds share the file description: EOF must not half-close the client.
4. **Retry**: idempotent + replayable body only. Do not widen it. Covered by e2e `http/retry/*`.
5. Pool: accept only a complete, keep-alive, framed response with no over-read. The per-origin cap must stay above the expected concurrency per origin: at 1024 concurrent connections a 256 cap collapsed throughput 25× (connection churn, 502s).
6. An epoll state-machine rewrite was tried and abandoned (same CPU/req as Tokio, worse rps). Do not return to it without new data.
7. Gains < 5 % = noise. Every performance claim goes through the fixed-seed matrix.
8. io_uring (monoio prototype, removed from the repo — recoverable at commit `a2b11e3`): ±8 % depending on size, no net gain on 1 core; kTLS: opt-in, wins on tunnels/RAM, loses on HTTP bulk. Do not change these decisions without a new measurement (multi-core).

## Known state

**Keep this section up to date: update it in the same commit whenever the project evolves (feature added, test counts, reference campaign, backlog change).**

- Last updated 2026-10-05, branch `main`. Linux + macOS, CI in `.github/workflows/` (lint, tests + e2e on Linux/macOS, installer; tag `vX.Y.Z` → release).
- `make check` green: fmt, clippy `-D warnings` (workspace, e2e included), `cargo test -p rustle` 75/75 (unit + in-process end-to-end), e2e `--quick` 68 scenarios. `make e2e` on macOS: 523 OK + 26 SKIP (Linux-only paths) out of 549; `robustness/huge-request-line` flaked once under full parallel load (passes alone), to watch.
- Implemented: HTTP/1.1 + CONNECT, HTTPS proxy (TLS, rustls), HTTP/2, SOCKS5 (TCP + UDP), shared-nothing shards with `SO_REUSEPORT` and optional core pinning, splice relay, chunked uploads, policy (destinations, client allowlist, bans, anti-slowloris), access log, opt-in kTLS, `rustle` CLI (init/check/status/reload/service/update/uninstall), runtime reload (control socket + SIGHUP), WebSocket upgrades (`http.allow_websocket`), `curl | sh` installer, black-box e2e suite.
- **The reference campaign below predates the fairness pass (3proxy NODELAY, Squid 7.7 + buffers, gost 3.3.0 + GOGC, Tinyproxy 1.11.3, rustle without native codegen): re-run `make bench-all` on the Linux harness, then `make charts` and refresh the README table, before quoting any competitor figure.
- Reference campaign: 2026-10-05 (`bench/results/campaign-20261005-120351.txt`, rewritten generator/origin harness, 1 core + 2-core harness). Its numbers feed the README charts and table (`make charts`); `bench/RESULTS.md` still holds the older 2026-10-03 campaign and must be refreshed. Produced by `bench/run-campaign.sh` (5 proxies × plain/TLS/h2), tables by `bench/tools/campaign-table.py`.
- WebSocket bench (`MATRIX=ws`): written and smoke-tested on macOS only (generator, origin, tools); never run on the Linux/Docker harness yet, no published numbers, and which competitors accept Upgrade is unknown until it runs.
- The bench (static origin, Content-Length) exercises neither chunked, SOCKS5 nor uploads: covered by the tests and the e2e suite.
- e2e suite: one expected behavior = one scenario. Bug found → first a scenario that reproduces it (it must fail), then the fix.

## Backlog / improvement areas

**Performance**
- **Multi-core: measure it** (`REMOTE=1 run-scaling.sh` on three machines, `bench/REMOTE.md`; the setup is written, never run yet) — nothing is measured beyond one core. Retest io_uring and kTLS in that setting.
- h2 path: slower than TLS+HTTP/1.1 on bulk (a `Bytes` copy per chunk, framing). h2 flood ahead of gost (4/16 MB windows), bound by the harness core.
- kTLS: understand the HTTP bulk loss (−37 % at 1 MB) — `perf` unavailable on the current box.
- Pool: no background reaper (a socket idle > 15 s is dropped when reached, a full origin evicts its oldest). DNS: full wipe when full, no negative cache, no coalescing.
- PGO/BOLT, `panic=abort` (rejected: a panic would kill the whole shard), `target-cpu=native` outside Docker.

**Functional (not supported)**
- Pipelined client requests (answered one at a time: close after the first response), `Upgrade` other than WebSocket and WebSocket over h2 (RFC 8441), full `Expect: 100-continue`, HTTP/3 to the proxy (MASQUE).
- Reload: restart-only keys (listeners, workers, pool, `[log]`, kTLS) still need a restart; open connections keep their old config.
- Upstream responses: duplicate `Content-Length` not checked (origin assumed trusted).

**Quality**
- CI (`make check`). `Mutex::lock().unwrap()` → poison = panic. No `setrlimit` (plan `LimitNOFILE` for deployment: ~6 fds per splice tunnel).
