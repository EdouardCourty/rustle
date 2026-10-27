# Results

Published measurements of rustle against 3proxy, Squid, gost and Tinyproxy.

## Environment

Three bare-metal machines (Scaleway Elastic Metal, Paris, hourly billing) in one Private Network, driven over SSH by `bench/run-remote.sh` (generator → proxy → origin and sink), containers on the host network. Setup, settings and validity rules: [`REMOTE.md`](REMOTE.md), [`RUNBOOK.md`](RUNBOOK.md), [`FAIRNESS.md`](FAIRNESS.md).

| Role | Server | CPU | Role in the run |
|---|---|---|---|
| Generator | EM-I320E-NVMe | AMD EPYC 8224P, 24 cores / 48 threads | `bench-client`, 24 threads |
| Proxy | EM-I320E-NVMe | AMD EPYC 8224P, 24 cores / 48 threads | proxy under test, **one core** (CPU 0, its hyperthread sibling offline, NIC interrupts moved to the other cores) |
| Origin + sink | EM-I220E-NVMe | AMD EPYC 8124P, 16 cores / 32 threads | static origin (8 threads) and discard sink (8 threads) |

* Network: 25 Gbit/s Private Network (VLAN on the NIC), MTU 1500, `iperf3 -P 4`: **23.5 Gbit/s** generator → proxy and proxy → origin. Bulk and tunnel tests that reach ~22 Gbit/s are limited by this link, not by the proxy.
* Software: Ubuntu 26.04.1 LTS, kernel 7.0.0-38-generic, Docker 29.1.3, governor `performance`, irqbalance off, identical sysctls on the three hosts (`bench/remote/bootstrap.sh`). Preflight (all three machines compared, links measured): `bench/results/preflight-20261028-142835.txt`, 0 differences.
* Commit: servers on `7d1c143`; the driving checkout was `0a77316` (only the Mac-side `bench/preflight.sh` differs).
* Protocol: auth on, 1 proxy core and 1 worker for every proxy, seed `2004113323` (shuffled test order, proxy restarted before each test), warmup run apart, median of 3 measured runs of 15 s, concurrency sweep per test (best clean level kept, `@conns`). Competitor versions and settings: `FAIRNESS.md`.
* Date: 2026-10-28.
* Raw data: every run behind the tables below is committed in [`published/2026-10-28/`](published/2026-10-28/) (indexes, `results.txt` of each run, preflight), with the commands to rebuild the tables.

## Chapter 1: plain HTTP proxy, 1 core

Campaign indexes `bench/results/campaign-20261028-143817.txt` (rustle, 3proxy, Squid, Tinyproxy) and `campaign-20261028-160509.txt` (gost), merged in `campaign-20261028-163500.txt`.

| Test | rustle | 3proxy | Squid | Tinyproxy | gost |
|---|---|---|---|---|---|
| HTTP 1 KB, req/s | **55,258** (18 µs/req, @64) | 21,032 (48 µs, @256) | 15,140 (66 µs, @64) | 7,580 (129 µs, @64) | 4,755 (210 µs, @64) |
| HTTP 10 KB, req/s | **40,783** (25 µs, @64) | 19,169 (52 µs, @256) | 10,814 (92 µs, @64) | 5,669 (174 µs, @64) | 4,436 (225 µs, @64) |
| 1 KB, no keep-alive, req/s | **27,827** (36 µs, @256) | 5,132 (192 µs, @256) | 7,804 (128 µs, @64) | 7,584 (129 µs, @64) | 4,780 (209 µs, @64) |
| HTTP 1 MB, req/s | **2,624** (@16) | 1,784 (@64) | 353 (@16) | 199 (@256) | 1,046 (@64) |
| HTTP 10 MB, req/s | **268** (@32, link-bound) | 191 (@8) | 33 (@8) | 21 (@32) | 145 (@32) |
| Tunnels (flood), Gbit/s | **22.0** (@8, link-bound) | 15.7 (@8) | 13.6 (@8) | 1.8 (@32) | 13.2 (@32) |
| Tunnels, proxy CPU per GB | **259 ms** | 524 ms | 605 ms | 4,684 ms | 623 ms |
| RAM, 1,000 idle tunnels | 14 MB | 104 MB | 150 MB | 18 MB | 161 MB |
| RAM, 5,000 idle tunnels | 40 MB | 499 MB | 275 MB | 61 MB | 653 MB |
| RAM, 10,000 idle tunnels | 54 MB | 992 MB | 432 MB | 115 MB | 1,243 MB |
| RAM, 25,000 idle tunnels | **95 MB** | 2,461 MB | 912 MB | 274 MB | 3,228 MB |

Validity (`tools/validity.py`): no client error, every tunnel opened, origin and sink never saturated, no OOM on the proxy host. Two rustle points are flagged "proxy under 90 % of its core": `flood` (70 %) and `http-10mb` (83 %). Both are at the link ceiling (~22 Gbit/s of the 23.5 measured), so adding connections cannot help; the true rustle figure for these two is at least the one shown. At 1 MB rustle sits at 95 % of its core, near the same ceiling.

Reading:

* Small requests are where the gap is large: 2.6× the next proxy (3proxy) at 1 KB, 2.1× at 10 KB, 3.6× without keep-alive (against Squid; 5.4× against 3proxy).
* Bulk transfers: rustle leads, but at 1 MB and especially 10 MB and in tunnels it reaches the link, so the measured margin is a lower bound.
* Tunnel CPU cost: rustle spends about half the CPU per GB of 3proxy (259 vs 524 ms), measured at the link ceiling for rustle and at the CPU ceiling for the others.
* Memory: at 25,000 idle tunnels the next best is Tinyproxy (274 MB), 2.9× rustle's 95 MB; the others are 10× to 34× larger.
* Tinyproxy has no upstream keep-alive (documented limitation), gost and 3proxy needed 256 to 1,024 connections to fill a core at 1 KB (hence the sweep).

Caveats: one campaign, one seed, 3 runs per point; noise is ±3–5 %, differences below that are not conclusions. The workload is a static origin with `Content-Length` (no chunked, no uploads, no SOCKS5). 1 core in plain HTTP only: TLS, HTTP/2, WebSocket and multi-core are separate chapters.

## Chapter 2: HTTPS proxy (TLS), 1 core

Index `campaign-20261028-203600.txt` (merged chapters 1 to 3). TLS 1.3, AES-128-GCM negotiated by every proxy (`tls_negotiated`), session resumption on for the main matrix, kTLS off (rustle's default; see the kTLS section). 3proxy and Tinyproxy have no TLS listener. There is no `splice` on a TLS port: the proxy decrypts in user space.

| Test | rustle | Squid | gost |
|---|---|---|---|
| HTTP 1 KB, req/s | **49,461** (20 µs/req, @64) | 13,509 (74 µs, @64) | 2,528 (395 µs, @64) |
| HTTP 10 KB, req/s | **35,460** (28 µs, @64) | 9,341 (107 µs, @64) | 2,433 (411 µs, @64) |
| 1 KB, no keep-alive, req/s | **6,119** (163 µs, @64) | 2,047 (489 µs, @64) | 2,516 (397 µs, @64) |
| HTTP 1 MB, req/s | **1,297** (@16) | 277 (@16) | 615 (@64) |
| HTTP 10 MB, req/s | **142** (@32) | 27 (@8) | 83 (@128) |
| Tunnels (flood), Gbit/s | **12.0** (688 ms/GB, @32) | 4.9 (1,688 ms/GB, @8) | 7.1 (1,172 ms/GB, @32) |
| RAM, 1,000 idle tunnels | 57 MB | 181 MB | 240 MB |
| RAM, 5,000 idle tunnels | 253 MB | 432 MB | 903 MB |
| RAM, 10,000 idle tunnels | 466 MB | 760 MB | 2,110 MB |
| RAM, 25,000 idle tunnels | **810 MB** | 2,584 MB | 4,094 MB |

TLS without session resumption (every connection a full handshake, `tlsfull`, 1 KB, no keep-alive, req/s): rustle **5,458** (183 µs), gost 2,246 (445 µs), Squid 1,922 (521 µs).

Reading: rustle keeps its lead over Squid and gost in TLS (3.7× Squid and 19.6× gost on 1 KB keep-alive, 2.4× to 2.8× on the full handshake), but the absolute figures fall well below plain HTTP (12.0 against 22.0 Gbit/s on tunnels, 1,297 against 2,624 req/s at 1 MB): without splice the data crosses user space. The memory advantage narrows too: about 32 KB per TLS tunnel (rustls buffers) against about 4 KB in plain HTTP, 810 MB against 2.6 GB (Squid) and 4.1 GB (gost) at 25,000 tunnels. gost does the same 2.5k req/s with or without keep-alive, so its overhead is per request, not per connection. No client error, every tunnel opened, harness not saturated (`validity.py` clean for TLS, tlsfull and h2).

## Chapter 3: HTTP/2 to the proxy, 1 core

h2 is supported by rustle and gost only (ALPN `h2`; one h2 connection per simulated connection, sequential requests; CONNECT is one stream on its own connection).

| Test | rustle | gost |
|---|---|---|
| HTTP 1 KB, req/s | **33,451** (30 µs/req, @64) | 5,231 (191 µs, @64) |
| HTTP 10 KB, req/s | **24,789** (40 µs, @64) | 4,732 (211 µs, @64) |
| 1 KB, no keep-alive, req/s | **4,974** (201 µs, @64) | 2,187 (457 µs, @64) |
| HTTP 1 MB, req/s | **860** (@16) | 447 (@16) |
| HTTP 10 MB, req/s | **88** (@8) | 50 (@8) |
| Tunnels (flood), Gbit/s | **8.8** (933 ms/GB, @8) | 7.8 (1,046 ms/GB, @8) |
| RAM, 1,000 idle tunnels | 70 MB | 225 MB |
| RAM, 5,000 idle tunnels | 330 MB | 1,245 MB |
| RAM, 10,000 idle tunnels | 705 MB | 2,639 MB |
| RAM, 25,000 idle tunnels | **1,263 MB** | 5,884 MB |

Reading: rustle's h2 path is about 32 % slower than its own TLS + HTTP/1.1 path on 1 KB (33.5k against 49.5k req/s) and 34 % at 1 MB: framing and one `Bytes` copy per chunk (known, in the backlog). It is still 6.4× gost on 1 KB and 4.7× on 25,000 tunnels in memory. On tunnels the two are close (8.8 against 7.8 Gbit/s, +13 %): above the ±3–5 % noise, but from a single campaign, so a small lead rather than a firm one.

## Chapter 4: WebSocket through the proxy, 1 core

`MATRIX=ws`: `GET` with `Upgrade: websocket` on the plain-HTTP path (and through the HTTPS proxy), then the origin echoes frames verbatim; `ws-64b` = 64 byte messages, 64 connections; `ws-64kb` = 64 KB frames, 8 connections; `ws-short` = one connection per exchange. Index `campaign-ws-20261028-214800.txt`. Msg/s, p99 latency and proxy CPU per message at the best clean level (all @64 for `ws-64b` and `ws-short`, @8 for `ws-64kb`).

Plain (WebSocket upgrade on the HTTP proxy port):

| Test | rustle | Squid | gost | 3proxy | Tinyproxy |
|---|---|---|---|---|---|
| 64 B messages, msg/s | **58,506** (p99 1.29 ms, 17 µs/msg) | 43,710 (1.49 ms, 23 µs) | 46,808 (2.57 ms, 21 µs) | 33,136 (3.16 ms, 30 µs) | unsupported |
| 64 KB frames, Gbit/s | **11.6** (666 ms/GB) | 6.6 (1,240 ms/GB) | 6.9 (1,182 ms/GB) | 7.9 (1,031 ms/GB) | unsupported |
| One connection per exchange, msg/s | **10,740** (p99 6.7 ms, 93 µs) | 4,879 (13.2 ms, 205 µs) | 3,726 (29.1 ms, 268 µs) | 251 (402.7 ms, 3,972 µs) | unsupported |

Through the HTTPS proxy (TLS):

| Test | rustle | gost | Squid |
|---|---|---|---|
| 64 B messages, msg/s | **56,955** (p99 1.31 ms) | 44,426 (2.70 ms) | 36,786 (1.77 ms) |
| 64 KB frames, Gbit/s | **5.8** (1,411 ms/GB) | 3.8 (2,161 ms/GB) | 2.4 (3,458 ms/GB) |
| One connection per exchange, msg/s | **4,462** (p99 15.1 ms) | 2,190 (44.0 ms) | 1,755 (37.0 ms) |

Reading: rustle leads in every WebSocket test: 1.3× to 1.8× on small messages, 1.5× to 1.8× on 64 KB frames (plain), and 2.2× to 2.9× (43× for 3proxy) when the connection itself is the cost. In TLS the margins are similar (1.3× to 1.6× small messages, 1.5× to 2.4× large frames). Tinyproxy removes the `Upgrade` header (answered 426): it does not support WebSocket and is listed as unsupported, not as zero. 3proxy supports it but spends about 4 ms of CPU per new connection. Validity: no client error, no saturated origin or sink, every point at the proxy's CPU limit (`validity.py` flags only Tinyproxy's unsupported points).

Squid is configured to pass WebSocket upgrades (`http_upgrade_request_protocols` with `allow all` for both `WebSocket` and `websocket`; authentication stays enforced by `http_access`, see [`FAIRNESS.md`](FAIRNESS.md)).

## Chapter 5: rustle with kTLS (opt-in), 1 core

`tls.ktls = true`: rustls does the handshake, then hands the session keys to the kernel (`TCP_ULP`, `SOL_TLS`), which encrypts and decrypts the records and lets `splice` serve the TLS port. The kernel module `tls` was loaded on the proxy host (`modprobe tls`) and the kernel counters confirm it was used (`TlsTxSw` and `TlsRxSw` 890,623, `TlsDecryptError` 0). Same matrix, seed and hosts as chapter 2; index `campaign-20261028-215104.txt`. No competitor offers an equivalent, so this is rustle against itself.

| Test | rustle, TLS in user space | rustle, kTLS | Change |
|---|---|---|---|
| HTTP 1 KB, req/s | 49,461 | 43,422 | −12 % |
| HTTP 10 KB, req/s | 35,460 | 31,154 | −12 % |
| 1 KB, no keep-alive, req/s | 6,119 | 5,158 | −16 % |
| HTTP 1 MB, req/s | 1,297 | 896 | −31 % |
| HTTP 10 MB, req/s | 142 | 89 | −37 % |
| Tunnels, Gbit/s | 12.0 (688 ms/GB) | 6.9 (1,183 ms/GB) | −42 % (+72 % CPU per GB) |
| RAM, 1,000 idle tunnels | 57 MB | 54 MB | −5 % |
| RAM, 5,000 idle tunnels | 253 MB | 228 MB | −10 % |
| RAM, 10,000 idle tunnels | 466 MB | 372 MB | −20 % |
| RAM, 25,000 idle tunnels | 810 MB | 753 MB | −7 % |

Reading: on this hardware and kernel (EPYC 8224P, Linux 7.0, software kernel TLS, no NIC offload: `TlsTxSw`, not `TlsTxDevice`) kTLS is slower than rustls on every throughput test, including tunnels, and saves 5 to 20 % of memory. It stays off by default. The cost is the kernel's software AES-GCM and record handling on the one proxy core against rustls' AES-NI implementation. With a NIC that offers TLS offload (`TlsTxDevice`) the result could differ: not tested.

## Chapter 6: multi-core scaling, plain HTTP 1 KB

The proxy gets n physical cores (`PROXY_CPUS=0-(n-1)`, `WORKERS=n`; the hyperthread siblings of those cores are offline and the NIC interrupts sit on the other cores, re-applied with `preflight.sh APPLY=1` for each n). Same hosts, seed and protocol as chapter 1, one test (`http-1kb`), connection levels 64, 256, 1,024 and 4,096 (`SWEEP_OVERRIDE`), best clean level kept. Indexes `campaign-mc-n1.txt` to `campaign-mc-n8.txt`. Tinyproxy is left out (one thread per connection, no core-count setting).

| Proxy cores | rustle | 3proxy | Squid | gost |
|---|---|---|---|---|
| 1 | **55,275** (@64) | 21,106 (@256) | 15,093 (@64) | 4,785 (@64) |
| 2 | **111,587** (@64) | 34,381 (@1,024) | 29,567 (@64) | 9,177 (@64) |
| 4 | **218,921** (@64) | 67,390 (@1,024) | 59,417 (@256) | 17,676 (@256) |
| 8 | **439,740** (@256) | 114,736 (@4,096) | 116,833 (@256) | 31,011 (@256) |
| Scaling 1 → 8 cores | ×7.96 | ×5.44 | ×7.74 | ×6.48 |

Reading: rustle scales linearly, about 55,000 req/s per added core, and at 8 cores serves 3.8× the next proxy (Squid 116,833 and 3proxy 114,736) and 14× gost. Squid scales almost as well as rustle but from a lower base (about 14,500 req/s per core); 3proxy needs far more connections to keep its cores busy (its best level moves from 256 to 4,096) and still falls short of linear (×5.4).

Validity (`tools/validity.py` on each n): no client error, every best level at the proxy's CPU limit (`proxy_util` 0.97 to 1.00), generator never saturated, origin at 72 % of its threads at the largest point (rustle, n=8) and 20 % or less for the others. The traffic stays near 4 Gbit/s at 440,000 req/s, far from the 23.5 Gbit/s link: this curve measures the proxies, not the network.
