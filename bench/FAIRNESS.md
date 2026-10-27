# Fairness of the comparison

Every proxy runs with the tuning **its own documentation recommends**, and each choice below was checked with an A/B before being kept. Each proxy has its own design (event loop, threads, processes, a Go runtime): the benchmark gives all of them the same cores, the same load and the same rules, and does not try to make them behave alike. If you know a better setting for one of them, open an issue or a pull request with the config and the numbers: it goes in, and the campaign is re-run.

Common to all proxies (identical harness for everyone): the proxy alone on its cores of a dedicated machine, the load generator and the origin and sink on two other machines (the published campaign, [`RESULTS.md`](RESULTS.md), [`REMOTE.md`](REMOTE.md)), same kernel and sysctls on the three hosts, authentication on with one key, access log off, upstream keep-alive wherever the proxy supports it, same TLS certificate type (ECDSA P-256), fixed seed and shuffled test order, proxy restarted before every test, median of 3 runs, `err_kinds` must be empty.

## What each proxy runs

Versions are those pinned when the campaign started (October 2026).


| Proxy         | Version         | How it is built                                                                            | Settings that go beyond the defaults                                                                                                                                                                                                                            |
| ------------- | --------------- | ------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **rustle**    | this repository | `cargo build --locked --release` (LTO, no `target-cpu`, `Cargo.lock`: as in `release.yml`) | `socket_buffer_bytes = 1048576` (also the shipped default); `SOCK_BUF=0` runs it with kernel autotuning like the others                                                                                                                                         |
| **3proxy**    | 1.0.0           | from source, `Makefile.Linux`                                                              | `proxy -a -osTCP_NODELAY -ocTCP_NODELAY` (documented for small exchanges; `-a` = no `Forwarded` header, like the others), `maxconn`, `nscache`, timeouts                                                                                                        |
| **Squid**     | 7.7             | from source, upstream's default flags, OpenSSL                                             | `cache deny all`, `cache_mem 0`, `tcp_recv_bufsize 1 MB`, `read_ahead_gap 1 MB`, `forwarded_for delete`, `via off`, auth helper results cached 2 h, `workers N` for N cores, `http_upgrade_request_protocols WebSocket` and `websocket` `allow all` (see below) |
| **gost**      | 3.3.0           | official image                                                                             | `GOGC=400`, logger level `error`; Go sizes `GOMAXPROCS` from the cpuset                                                                                                                                                                                         |
| **Tinyproxy** | 1.11.3          | from source                                                                                | `MaxClients 200000`, `LogLevel Critical`, no `Via`, no filters                                                                                                                                                                                                  |


## The A/B behind the settings

Measured on a single development host, one core for the proxy and one for the generator, 64 keep-alive connections, 1 KB responses, 6 s per point, several passes. These runs only **choose** the settings; the published numbers come from the three-machine campaign.

**3proxy** (without any socket option it sits in Nagle + a 40 ms delayed ACK, ~1.5k req/s at 1 KB):


| Variant                                    | req/s     | p50   |
| ------------------------------------------ | --------- | ----- |
| default config                             | 1,510     | 42 ms |
| `**-osTCP_NODELAY -ocTCP_NODELAY**` (kept) | **5,490** | 11 ms |
| + `authcache user,pass`                    | 5,530     | 11 ms |
| + grace delay `-g8000,3,10`                | 3,430     | 11 ms |
| + `stacksize 65536`                        | 3,700     | 11 ms |


The default config therefore measures a missing documented option, not 3proxy's speed. Short connections (`--no-keepalive`) are unaffected by the option.

**gost** (`GOGC` sweep, and the HTTP handler options):


| Variant                         | req/s     | RAM                                              |
| ------------------------------- | --------- | ------------------------------------------------ |
| default (`GOGC=100`)            | 7,650     | not measured                                     |
| `GOGC=200`                      | 8,530     | 40 MB                                            |
| `**GOGC=400**` (kept)           | **8,760** | 62 MB                                            |
| `GOGC=800`                      | 8,840     | 110 MB                                           |
| `GOGC=1600`                     | 9,160     | 210 MB                                           |
| `GOGC=off`                      | 7,130     | unbounded                                        |
| `GOMAXPROCS=1`                  | 7,500     | (same as default: already sized from the cpuset) |
| handler option `keepalive=true` | 4,960     | (slower than the default, so not used)           |


gost reopens an upstream connection for almost every proxied request either way (1.00 per request with the default handler, 0.82 with `keepalive=true` in the A/B; its HTTP handler disables keep-alive by default). That is gost's behaviour, not a missing setting; it was not re-measured on the three machines. `GOST_LOGGER_LEVEL=error` is honoured by the image (0 log lines per run against ~96,000 without it). `GOGC=400` keeps most of the gain for a moderate memory cost; the 25,000-tunnel RAM test would punish a larger value.

**Squid** (5.7 from the Debian package for the directive A/B below; 7.7 built from source with the kept directives measured +19 % over 5.7 with the same directives):


| Variant                                                                    | 1 KB req/s | 1 MB req/s |
| -------------------------------------------------------------------------- | ---------- | ---------- |
| baseline                                                                   | 31,400     | 890        |
| `memory_pools off`, `dns_v4_first on`, `half_closed_clients off`           | 28,500     | 905        |
| `**tcp_recv_bufsize 1 MB` + `read_ahead_gap 1 MB`** (kept)                 | 32,300     | 1,020      |
| `pipeline_prefetch 0`, persistent connections on, `cache_log /dev/null`, … | 32,400     | 887        |


Two checks on the published campaign:

- **Authentication cost.** Squid at 1 KB with the auth helper and without (three-machine setup, same core): 15,140 against 15,395 req/s at 64 connections, 14,511 against 14,976 at 256, 10,052 against 10,599 at 1,024 (+2 % to +5 % without). Authentication is not what limits Squid there.
- **WebSocket upgrades.** Squid drops an `Upgrade` offer unless `http_upgrade_request_protocols` allows it. The directive takes fast ACLs only (so not a `proxy_auth` ACL) and protocol names are case sensitive (`WebSocket` and the `websocket` clients send). The config lists both spellings with `allow all`; authentication stays enforced by `http_access`.

## Settings looked at and not applied

- Anything that disables or changes what the proxy does for every request (auth caching tricks that skip password checks, `fakeresolve`, `ip` auth caches).
- Larger `GOGC`/`stacksize` values that trade memory for a few percent.
- Kernel-level tuning (`sysctl`) beyond what is common to all proxies.
- Splice for 3proxy: not built by default in the version tested.
- kTLS for rustle in the main TLS tables: it is opt-in (off by default) and no competitor has an equivalent, so it is measured separately (`RESULTS.md`, kTLS chapter).

## Known asymmetries

- **rustle's socket buffers.** rustle ships with fixed 1 MB buffers; Squid gets the same 1 MB through its directives; 3proxy, gost and Tinyproxy have no such option and use kernel autotuning. `SOCK_BUF=0` runs rustle without it: on the A/B host it changes nothing measurable (194k vs 197k req/s at 1 KB, 5.8k vs 5.95k at 1 MB).
- **Tinyproxy** closes the upstream connection after every request; the generator replays an idempotent GET once on a reused connection closed without notice (counted in `reconnects`).
- **Features differ**: only rustle, Squid and gost speak TLS to the client, and only rustle and gost HTTP/2; 3proxy and Tinyproxy appear in the plain tables only. Tinyproxy does not support WebSocket upgrades (it removes `Upgrade`): it is listed as unsupported, never as zero.
- **One core, plus a multi-core curve.** The main matrix gives every proxy a single core. The multi-core curve (1, 2, 4 and 8 cores, `http-1kb` only, `RESULTS.md` chapter 6) uses the same cores for everybody (`PROXY_CPUS=0-(n-1)`, `WORKERS=n`): rustle runs `n` shards and Squid `workers n`; 3proxy and Tinyproxy are thread-per-connection and gost sizes `GOMAXPROCS` from the cpuset, so they use the `n` cores without a setting. Tinyproxy was left out of the curve; TLS, HTTP/2 and tunnels were not measured on several cores. The connection levels go up to 4,096 (`SWEEP_OVERRIDE`) because a multi-core proxy needs many more connections to saturate its cores.
- **The network is a ceiling.** The three machines are linked at 25 Gbit/s (23.5 measured with `iperf3`). A proxy that reaches that ceiling (rustle at 1 MB, 10 MB and in plain tunnels) shows the link, not its own limit: the table marks those points and its figure is a lower bound.

## Run rules

- **Every proxy at its own best concurrency.** Each load test runs at several connection counts (`sweep_levels` in `matrix.sh`: 64/256/1024 for small objects, 16/64/256 for 1 MB, 8/32/128 for 10 MB, 8/32 for tunnels) and the published figure is the best level without client errors. A fixed count understated 3proxy 5.8× (19 % CPU at 64 connections, 99 % at 1024). `NO_SWEEP=1` disables it.
- **Saturation is measured, not assumed.** Every result carries `proxy_util` (proxy CPU / cores given), `origin_util` and `sink_util`; `proxy_unsaturated=1` (below 90 % at the level shown) or `origin_saturated` / `sink_saturated` (above 90 %) mean the number is limited by the harness, by the link or by a latency-bound proxy, not by the CPU of the proxy. `tools/validity.py` lists those points before anything is published.
- **Same cores for everybody.** `run-remote.sh` refuses to start without `PROXY_CPUS` (0 = one core, 0-3 = four) and requires `WORKERS` to equal that core count. The proxy's hyperthread siblings are offline and the NIC interrupts sit on other cores (`preflight.sh APPLY=1`).
- **A run is invalid, and the script exits non-zero, when** any client-side error kind appears (`err_kinds` not empty), a hold test opens fewer tunnels than asked (a kernel or harness limit, never a proxy result), or the generator saturates. The generator also checks each response body against the exact size of the object it asked for.
- **The three machines are compared before every campaign** (sysctls, governor, MTU, offloads, kernel, Docker, commit) and the links measured (`preflight.sh`); a difference or a link below the minimum stops the run.
- **Thread-per-connection proxies** get `vm.max_map_count`, `pid_max`, `threads-max` raised and an unlimited container pids limit, the same for every proxy.
- Check `dmesg | grep -i oom` on the proxy host after every hold test.
- A step that fails to start is re-run alone; a missing result is listed as unsupported or failed, never as zero.

## TLS: resumed and full handshakes

The short-connection TLS test runs twice: with session resumption (a client that remembers its tickets) and without (`TLS_RESUME=0`, every connection a full handshake with the certificate signature; campaign mode `tlsfull`). The client's ticket store never runs dry: rustls' default keeps 8 tickets per server, which at 64 parallel connections made the resumed share depend on how fast each proxy was, so the store holds every ticket in flight (resumed share 100 % for all three). CPU per new connection on the A/B host (1 core, 64 connections):


| Proxy      | resumed | full handshake |
| ---------- | ------- | -------------- |
| rustle     | 83 µs   | 97 µs          |
| Squid 7.7  | 198 µs  | 220 µs         |
| gost 3.3.0 | 207 µs  | 232 µs         |


The three-machine figures are in `RESULTS.md`.

## Known limits of this benchmark

- Latency is closed-loop at each concurrency level (no open-loop rate); CPU per request is measured at saturation, not at an equal offered load.
- One campaign, one seed, three repeats, medians, noise about ±3 to 5 %; no confidence intervals.
- DNS is resolved once by the generator, IPv6, many origins, large header sets, chunked bodies, uploads and SOCKS5 are not part of the matrix (they are covered functionally by `e2e/`). The origin is static with `Content-Length`.
- TLS footing is checked, not assumed: the generator offers AES-128-GCM first (as hardware-AES clients do), logs the version/cipher negotiated and the share of resumed handshakes on every TLS result, and `tools/validity.py` flags proxies that differ. rustle, Squid 7.7 and gost all negotiate TLS 1.3 / AES-128-GCM; with the generator's default order (ring: AES-256 first) Squid negotiated AES-256-GCM, which is why the order is set.
- The HTTP/2 test opens one connection per client connection and sends sequential requests (no multiplexing); gost's h2 is a dedicated listener, rustle's is ALPN on the TLS port.
- Proxy CPU is the container cgroup's (user + system); softirq time spent on other cores is not attributed.
- Flood counts bytes written by the client and measures the upload direction only.

## Checklist when a version or a config changes

1. Update the version in the table above and in the Dockerfile.
2. A/B the change before keeping it; put the numbers in this file.
3. Re-run the campaign on the three-machine setup ([`RUNBOOK.md`](RUNBOOK.md), `make bench-publish`), regenerate the charts and the README table, and refresh `published/`. Never mix numbers from two campaigns.

