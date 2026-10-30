# Results

Published measurements of rustle against 3proxy, Squid, gost and Tinyproxy. Every table and every number below is generated from [`data/2026-10-28/benchmarks.json`](../data/2026-10-28/benchmarks.json) and [`campaign.json`](../data/2026-10-28/campaign.json) by `tools/render.py` (`make data`): nothing is typed by hand, see [`data/README.md`](../data/README.md).

## Environment

Three bare-metal machines (<!--n c hardware.provider-->Scaleway Elastic Metal, Paris<!--/n-->, hourly billing) in one Private Network, driven over SSH by `benchmarks/scripts/run-remote.sh` (generator → proxy → origin and sink), containers on the host network. Setup, settings and validity rules: [`REMOTE.md`](REMOTE.md), [`RUNBOOK.md`](RUNBOOK.md), [`FAIRNESS.md`](FAIRNESS.md).

<!-- bench:environment:start -->

| Role | Server | CPU | Role in the run |
|---|---|---|---|
| Generator | EM-I320E-NVMe | AMD EPYC 8224P, 24 cores / 48 threads | bench-client, 24 threads |
| Proxy | EM-I320E-NVMe | AMD EPYC 8224P, 24 cores / 48 threads | proxy under test, one core (CPU 0, its hyperthread sibling offline, NIC interrupts moved to the other cores) |
| Origin + sink | EM-I220E-NVMe | AMD EPYC 8124P, 16 cores / 32 threads | static origin (8 threads) and discard sink (8 threads) |

<!-- bench:environment:end -->

* Network: <!--n c network.kind-->25 Gbit/s Private Network (VLAN on the NIC)<!--/n-->, MTU <!--n c network.mtu-->1500<!--/n-->, `<!--n c network.measured_with-->iperf3 -P 4<!--/n-->`: **<!--n c network.measured_gbit_s-->23.5<!--/n--> Gbit/s** generator → proxy and proxy → origin. Where rustle gets close to it in the bulk and tunnel tests (~<!--n fmt plain-flood.rustle .0f-->22<!--/n--> Gbit/s), the test network sets the number, not the proxy.
* Software: <!--n c software.os-->Ubuntu 26.04.1 LTS<!--/n-->, kernel <!--n c software.kernel-->7.0.0-38-generic<!--/n-->, Docker <!--n c software.docker-->29.1.3<!--/n-->, governor `<!--n c software.governor-->performance<!--/n-->`, irqbalance off, identical sysctls on the three hosts (`benchmarks/scripts/remote/bootstrap.sh`). Preflight (all three machines compared, links measured): [`published/2026-10-28/preflight.txt`](../published/2026-10-28/preflight.txt).
* Commit: servers on `<!--n c commit-->7d1c143<!--/n-->`; the driving checkout was `0a77316` (only the Mac-side `benchmarks/scripts/preflight.sh` differs).
* Protocol: auth on, <!--n c proxy_cores-->1<!--/n--> proxy core and <!--n c proxy_cores-->1<!--/n--> worker for every proxy (rustle with `socket_buffer_bytes = 1048576` set in its bench config; its shipped default is kernel autotuning), seed `<!--n c seed-->2004113323<!--/n-->` (shuffled test order, proxy restarted before each test), warmup run apart, median of <!--n c repeats-->3<!--/n--> measured runs of <!--n c run_seconds-->15<!--/n--> s, concurrency sweep per test (best clean level kept, `@conns`). Competitor versions and settings: [`FAIRNESS.md`](FAIRNESS.md).
* Date: <!--n c date-->2026-10-28<!--/n-->.
* Units: memory is the peak RSS of the proxy (`VmHWM`) in MiB, written MB below; "ms/GB" is proxy CPU milliseconds per 10^9 bytes moved, GB being the decimal unit of the Gbit/s. `<90 % CPU` marks a point where the proxy used less than 90 % of its core at its best level: something other than the proxy's CPU set the number (the test network, the load generator, or the proxy waiting on latency).
* Raw data: every run behind the tables is committed in [`published/2026-10-28/`](../published/2026-10-28/) (`run.json` of each run, and the preflight); the manifest of the runs is the `sources` of [`campaign.json`](../data/2026-10-28/campaign.json).

## Chapter 1: plain HTTP proxy, 1 core

The `main` runs with the `plain` transport.

<!-- bench:plain:start -->

| Test | rustle | 3proxy | Squid | Tinyproxy | gost |
|---|---|---|---|---|---|
| HTTP 1 KB, req/s | **55,258** (18 µs/req, @64) | 21,032 (48 µs, @256) | 15,140 (66 µs, @64) | 7,580 (129 µs, @64) | 4,755 (210 µs, @64) |
| HTTP 10 KB, req/s | **40,783** (25 µs/req, @64) | 19,169 (52 µs, @256) | 10,814 (92 µs, @64) | 5,669 (174 µs, @64) | 4,436 (225 µs, @64) |
| 1 KB, no keep-alive, req/s | **27,827** (36 µs/req, @256) | 5,132 (192 µs, @256) | 7,804 (128 µs, @64) | 7,584 (129 µs, @64) | 4,780 (209 µs, @64) |
| HTTP 1 MB, req/s | **2,624** (@16) | 1,784 (@64) | 353 (@16) | 199 (@256) | 1,046 (@64) |
| HTTP 10 MB, req/s | **268** (@32, <90 % CPU) | 191 (@8) | 33 (@8) | 21 (@32) | 145 (@32) |
| Tunnels (flood), Gbit/s | **22.0** (@8, <90 % CPU) | 15.7 (@8) | 13.6 (@8) | 1.8 (@32) | 13.2 (@32) |
| Tunnels, proxy CPU per GB | **253 ms** | 511 ms | 591 ms | 4,574 ms | 609 ms |
| RAM, 1,000 idle tunnels | 14 MB | 104 MB | 150 MB | 18 MB | 161 MB |
| RAM, 5,000 idle tunnels | 40 MB | 499 MB | 275 MB | 61 MB | 653 MB |
| RAM, 10,000 idle tunnels | 54 MB | 992 MB | 432 MB | 115 MB | 1,243 MB |
| RAM, 25,000 idle tunnels | **95 MB** | 2,461 MB | 912 MB | 274 MB | 3,228 MB |

<!-- bench:plain:end -->

Validity: no client error, every tunnel opened, origin and sink never overloaded, no OOM on the proxy host. Two rustle points use less than 90 % of the proxy's core (`<90 % CPU` in the table): `flood` (<!--n util plain-flood.rustle-->70 %<!--/n-->) and `http-10mb` (<!--n util plain-10mb.rustle-->83 %<!--/n-->). In both, rustle gets about <!--n fmt plain-flood.rustle .0f-->22<!--/n--> of the <!--n c network.measured_gbit_s-->23.5<!--/n--> Gbit/s the test network can carry: the network is the limit, so adding connections cannot help and rustle can go faster than shown. At 1 MB rustle uses <!--n util plain-1mb.rustle-->95 %<!--/n--> of its core and is close to the same limit.

Reading:

* Small requests are where the gap is large: <!--n x plain-1kb.rustle plain-1kb.3proxy-->2.6×<!--/n--> the next proxy (3proxy) at 1 KB, <!--n x plain-10kb.rustle plain-10kb.3proxy-->2.1×<!--/n--> at 10 KB, <!--n x plain-short.rustle plain-short.squid-->3.6×<!--/n--> without keep-alive (against Squid; <!--n x plain-short.rustle plain-short.3proxy-->5.4×<!--/n--> against 3proxy).
* Bulk transfers: rustle leads, but at 1 MB and especially 10 MB and in tunnels it gets close to what the test network can carry, so its real lead is higher than the one measured.
* Tunnel CPU cost: rustle spends about half the CPU per GB of 3proxy (<!--n fmt plain-flood.rustle.cpu_ms_per_gb .0f-->253<!--/n--> vs <!--n fmt plain-flood.3proxy.cpu_ms_per_gb .0f-->511<!--/n--> ms), measured with rustle limited by the test network and the others limited by their own CPU.
* Memory: at 25,000 idle tunnels the next best is Tinyproxy (<!--n v plain-ram-25000.tinyproxy-->274 MB<!--/n-->), <!--n x plain-ram-25000.tinyproxy plain-ram-25000.rustle-->2.9×<!--/n--> rustle's <!--n v plain-ram-25000.rustle-->95 MB<!--/n-->; the others are <!--n xi plain-ram-25000.squid plain-ram-25000.rustle-->10×<!--/n--> to <!--n xi plain-ram-25000.gost plain-ram-25000.rustle-->34×<!--/n--> larger.
* Tinyproxy has no upstream keep-alive (documented limitation). 3proxy needed <!--n at plain-1kb.3proxy-->256<!--/n--> connections to fill a core at 1 KB (see the A/B of [`FAIRNESS.md`](FAIRNESS.md)); the other proxies peak at <!--n at plain-1kb.rustle-->64<!--/n--> (hence the sweep).

Caveats: one campaign, one seed, <!--n c repeats-->3<!--/n--> runs per point; noise is ±<!--n c noise_pct.min-->3<!--/n-->–<!--n c noise_pct.max-->5<!--/n--> %, differences below that are not conclusions. The workload is a static origin with `Content-Length` (no chunked, no uploads, no SOCKS5). <!--n c proxy_cores-->1<!--/n--> core in plain HTTP only: TLS, HTTP/2, WebSocket and multi-core are separate chapters.

## Chapter 2: HTTPS proxy (TLS), 1 core

The `main` runs with the `tls` and `tlsfull` transports. TLS 1.3, AES-128-GCM negotiated by every proxy (`tls_negotiated`), session resumption on for the main matrix, kTLS off (rustle's default; see the kTLS section). 3proxy and Tinyproxy have no TLS listener. There is no `splice` on a TLS port: the proxy decrypts in user space.

<!-- bench:tls:start -->

| Test | rustle | Squid | gost |
|---|---|---|---|
| HTTP 1 KB, req/s | **49,461** (20 µs/req, @64) | 13,509 (74 µs, @64) | 2,528 (395 µs, @64) |
| HTTP 10 KB, req/s | **35,460** (28 µs/req, @64) | 9,341 (107 µs, @64) | 2,433 (411 µs, @64) |
| 1 KB, no keep-alive, req/s | **6,119** (163 µs/req, @64) | 2,047 (489 µs, @64) | 2,516 (397 µs, @64) |
| HTTP 1 MB, req/s | **1,297** (@16) | 277 (@16) | 615 (@64) |
| HTTP 10 MB, req/s | **142** (@32) | 27 (@8) | 83 (@128) |
| Tunnels (flood), Gbit/s | **12.0** (672 ms/GB, @32) | 4.9 (1,649 ms/GB, @8) | 7.1 (1,145 ms/GB, @32) |
| RAM, 1,000 idle tunnels | 57 MB | 181 MB | 240 MB |
| RAM, 5,000 idle tunnels | 253 MB | 432 MB | 903 MB |
| RAM, 10,000 idle tunnels | 466 MB | 760 MB | 2,110 MB |
| RAM, 25,000 idle tunnels | **810 MB** | 2,584 MB | 4,094 MB |

<!-- bench:tls:end -->

TLS without session resumption (every connection a full handshake, `tlsfull`, 1 KB, no keep-alive, req/s): rustle **<!--n v tls-short-full.rustle-->5,458<!--/n-->** (<!--n us tls-short-full.rustle.cpu_us-->183 µs<!--/n-->), gost <!--n v tls-short-full.gost-->2,246<!--/n--> (<!--n us tls-short-full.gost.cpu_us-->445 µs<!--/n-->), Squid <!--n v tls-short-full.squid-->1,922<!--/n--> (<!--n us tls-short-full.squid.cpu_us-->521 µs<!--/n-->).

Reading: rustle keeps its lead over Squid and gost in TLS (<!--n x tls-1kb.rustle tls-1kb.squid-->3.7×<!--/n--> Squid and <!--n x tls-1kb.rustle tls-1kb.gost-->19.6×<!--/n--> gost on 1 KB keep-alive, <!--n xrange tls-short-full rustle gost squid-->2.4× to 2.8×<!--/n--> on the full handshake), but the absolute figures fall well below plain HTTP (<!--n v tls-flood.rustle-->12.0<!--/n--> against <!--n v plain-flood.rustle-->22.0<!--/n--> Gbit/s on tunnels, <!--n v tls-1mb.rustle-->1,297<!--/n--> against <!--n v plain-1mb.rustle-->2,624<!--/n--> req/s at 1 MB): without splice the data crosses user space. The memory advantage narrows too: about <!--n kb_per tls-ram-25000.rustle 25000-->33<!--/n--> KB per TLS tunnel (rustls buffers) against about <!--n kb_per plain-ram-25000.rustle 25000-->4<!--/n--> KB in plain HTTP, <!--n v tls-ram-25000.rustle-->810 MB<!--/n--> against <!--n mem tls-ram-25000.squid-->2.5 GB<!--/n--> (Squid) and <!--n mem tls-ram-25000.gost-->4.0 GB<!--/n--> (gost) at 25,000 tunnels. gost does the same <!--n k tls-1kb.gost-->2.5k<!--/n--> req/s with or without keep-alive, so its overhead is per request, not per connection. No client error, every tunnel opened, harness not saturated (clean for TLS, tlsfull and h2).

## Chapter 3: HTTP/2 to the proxy, 1 core

Only rustle and gost accept HTTP/2 from the client (ALPN `h2`); 3proxy, Squid and Tinyproxy have no HTTP/2 listener, so they are not in this table. One h2 connection per simulated connection, sequential requests; CONNECT is one stream on its own connection. The `main` runs with the `h2` transport.

<!-- bench:h2:start -->

| Test | rustle | gost |
|---|---|---|
| HTTP 1 KB, req/s | **33,451** (30 µs/req, @64) | 5,231 (191 µs, @64) |
| HTTP 10 KB, req/s | **24,789** (40 µs/req, @64) | 4,732 (211 µs, @64) |
| 1 KB, no keep-alive, req/s | **4,974** (201 µs/req, @64) | 2,187 (457 µs, @64) |
| HTTP 1 MB, req/s | **860** (@16) | 447 (@16) |
| HTTP 10 MB, req/s | **88** (@8) | 50 (@8) |
| Tunnels (flood), Gbit/s | **8.8** (911 ms/GB, @8) | 7.8 (1,022 ms/GB, @8) |
| RAM, 1,000 idle tunnels | 70 MB | 225 MB |
| RAM, 5,000 idle tunnels | 330 MB | 1,245 MB |
| RAM, 10,000 idle tunnels | 705 MB | 2,639 MB |
| RAM, 25,000 idle tunnels | **1,263 MB** | 5,884 MB |

<!-- bench:h2:end -->

Reading: rustle's h2 path is about <!--n apct h2-1kb.rustle tls-1kb.rustle-->32 %<!--/n--> slower than its own TLS + HTTP/1.1 path on 1 KB (<!--n k h2-1kb.rustle-->33.5k<!--/n--> against <!--n k tls-1kb.rustle-->49.5k<!--/n--> req/s) and <!--n apct h2-1mb.rustle tls-1mb.rustle-->34 %<!--/n--> at 1 MB: framing and one `Bytes` copy per chunk (known, in the backlog). It is still <!--n x h2-1kb.rustle h2-1kb.gost-->6.4×<!--/n--> gost on 1 KB and <!--n x h2-ram-25000.gost h2-ram-25000.rustle-->4.7×<!--/n--> on 25,000 tunnels in memory. On tunnels the two are close (<!--n v h2-flood.rustle-->8.8<!--/n--> against <!--n v h2-flood.gost-->7.8<!--/n--> Gbit/s, <!--n pct h2-flood.rustle h2-flood.gost-->+12 %<!--/n-->): above the ±<!--n c noise_pct.min-->3<!--/n-->–<!--n c noise_pct.max-->5<!--/n--> % noise, but from a single campaign, so a small lead rather than a firm one.

## Chapter 4: WebSocket through the proxy, 1 core

`MATRIX=ws`: `GET` with `Upgrade: websocket` on the plain-HTTP path (and through the HTTPS proxy), then the origin echoes frames verbatim; `ws-64b` = 64 byte messages, 64 connections; `ws-64kb` = 64 KB frames, 8 connections; `ws-short` = one connection per exchange. The `ws` runs. Msg/s, p99 latency and proxy CPU per message at each proxy's best clean level (the connection levels are in `benchmarks.json`).

Plain (WebSocket upgrade on the HTTP proxy port):

<!-- bench:ws:start -->

| Test | rustle | Squid | gost | 3proxy | Tinyproxy |
|---|---|---|---|---|---|
| 64 B messages, msg/s | **58,506** (p99 1.29 ms, 17 µs/msg) | 43,710 (1.49 ms, 23 µs) | 46,808 (2.57 ms, 21 µs) | 33,136 (3.16 ms, 30 µs) | unsupported |
| 64 KB frames, Gbit/s | **11.6** (650 ms/GB) | 6.6 (1,211 ms/GB) | 6.9 (1,154 ms/GB) | 7.9 (1,007 ms/GB) | unsupported |
| One connection per exchange, msg/s | **10,740** (p99 6.7 ms, 93 µs) | 4,879 (13.2 ms, 205 µs) | 3,726 (29.1 ms, 268 µs) | 251 (402.7 ms, 3,972 µs) | unsupported |

<!-- bench:ws:end -->

Through the HTTPS proxy (TLS):

<!-- bench:ws-tls:start -->

| Test | rustle | gost | Squid |
|---|---|---|---|
| 64 B messages, msg/s | **56,955** (p99 1.31 ms) | 44,426 (2.70 ms) | 36,786 (1.77 ms) |
| 64 KB frames, Gbit/s | **5.8** (1,378 ms/GB) | 3.8 (2,110 ms/GB) | 2.4 (3,377 ms/GB) |
| One connection per exchange, msg/s | **4,462** (p99 15.1 ms) | 2,190 (44.0 ms) | 1,755 (37.0 ms) |

<!-- bench:ws-tls:end -->

Reading: rustle leads in every WebSocket test: <!--n xrange ws-64b-rps rustle squid gost 3proxy-->1.2× to 1.8×<!--/n--> on small messages, <!--n xrange ws-64kb-gbps rustle squid gost 3proxy-->1.5× to 1.8×<!--/n--> on 64 KB frames (plain), and <!--n xrange ws-short rustle squid gost-->2.2× to 2.9×<!--/n--> (<!--n xi ws-short.rustle ws-short.3proxy-->43×<!--/n--> for 3proxy) when the connection itself is the cost. In TLS the margins are similar (<!--n xrange ws-tls-64b-rps rustle gost squid-->1.3× to 1.5×<!--/n--> small messages, <!--n xrange ws-tls-64kb-gbps rustle gost squid-->1.5× to 2.4×<!--/n--> large frames). Tinyproxy removes the `Upgrade` header (answered 426): it does not support WebSocket and is listed as unsupported, not as zero. 3proxy supports it but spends about <!--n ms ws-short.3proxy.cpu_us-->4 ms<!--/n--> of CPU per new connection. Validity: no client error, no saturated origin or sink, every point at the proxy's CPU limit (only Tinyproxy's points are marked unsupported).

Squid is configured to pass WebSocket upgrades (`http_upgrade_request_protocols` with `allow all` for both `WebSocket` and `websocket`; authentication stays enforced by `http_access`, see [`FAIRNESS.md`](FAIRNESS.md)).

## Chapter 5: rustle with kTLS (opt-in), 1 core

`tls.ktls = true`: rustls does the handshake, then hands the session keys to the kernel (`TCP_ULP`, `SOL_TLS`), which encrypts and decrypts the records and lets `splice` serve the TLS port. The kernel module `tls` was loaded on the proxy host (`modprobe tls`) and the kernel counters confirm it was used (`TlsTxSw` and `TlsRxSw` counted the records, `TlsDecryptError` stayed at 0). Same matrix, seed and hosts as chapter 2 (the `ktls` runs of the campaign). No competitor offers an equivalent, so this is rustle against itself.

<!-- bench:ktls:start -->

| Test | rustle, TLS in user space | rustle, kTLS | Change |
|---|---|---|---|
| HTTP 1 KB, req/s | 49,461 | 43,422 | −12 % |
| HTTP 10 KB, req/s | 35,460 | 31,154 | −12 % |
| 1 KB, no keep-alive, req/s | 6,119 | 5,158 | −16 % |
| HTTP 1 MB, req/s | 1,297 | 896 | −31 % |
| HTTP 10 MB, req/s | 142 | 89 | −37 % |
| Tunnels, Gbit/s | 12.0 (672 ms/GB) | 6.9 (1,155 ms/GB) | −43 % (+72 % CPU per GB) |
| RAM, 1,000 idle tunnels | 57 MB | 54 MB | −5 % |
| RAM, 5,000 idle tunnels | 253 MB | 228 MB | −10 % |
| RAM, 10,000 idle tunnels | 466 MB | 372 MB | −20 % |
| RAM, 25,000 idle tunnels | 810 MB | 753 MB | −7 % |

<!-- bench:ktls:end -->

Reading: on this hardware and kernel (EPYC 8224P, Linux 7.0, software kernel TLS, no NIC offload: `TlsTxSw`, not `TlsTxDevice`) kTLS is slower than rustls on every throughput test, including tunnels, and saves <!--n ktrange-->5 to 20 %<!--/n--> of memory. It stays off by default. The cost is the kernel's software AES-GCM and record handling on the one proxy core against rustls' AES-NI implementation. With a NIC that offers TLS offload (`TlsTxDevice`) the result could differ: not tested.

## Chapter 6: multi-core scaling, plain HTTP 1 KB

The proxy gets n physical cores (`PROXY_CPUS=0-(n-1)`, `WORKERS=n`; the hyperthread siblings of those cores are offline and the NIC interrupts sit on the other cores, re-applied with `preflight.sh APPLY=1` for each n). Same hosts, seed and protocol as chapter 1, one test (`http-1kb`), connection levels 64, 256, 1,024 and 4,096 (`SWEEP_OVERRIDE`), best clean level kept (the `scaling` runs of the campaign). Tinyproxy is left out (one thread per connection, no core-count setting).

<!-- bench:scaling:start -->

| Proxy cores | rustle | 3proxy | Squid | gost |
|---|---|---|---|---|
| 1 | **55,275** (@64) | 21,106 (@256) | 15,094 (@64) | 4,785 (@64) |
| 2 | **111,587** (@64) | 34,381 (@1,024) | 29,568 (@64) | 9,178 (@64) |
| 4 | **218,922** (@64) | 67,390 (@1,024) | 59,418 (@256) | 17,677 (@256) |
| 8 | **439,741** (@256) | 114,736 (@4,096) | 116,834 (@256) | 31,011 (@256) |
| Scaling 1 → 8 cores | ×7.96 | ×5.44 | ×7.74 | ×6.48 |

<!-- bench:scaling:end -->

Reading: rustle scales linearly, about <!--n percore S8.rustle 8 1000-->55,000<!--/n--> req/s per added core, and at 8 cores serves <!--n x S8.rustle S8.squid-->3.8×<!--/n--> the next proxy (Squid <!--n fmt S8.squid ,.0f-->116,834<!--/n--> and 3proxy <!--n fmt S8.3proxy ,.0f-->114,736<!--/n-->) and <!--n xi S8.rustle S8.gost-->14×<!--/n--> gost. Squid scales almost as well as rustle but from a lower base (about <!--n percore S8.squid 8 100-->14,600<!--/n--> req/s per core); 3proxy needs far more connections to keep its cores busy (its best level moves from <!--n at S1.3proxy-->256<!--/n--> to <!--n at S8.3proxy-->4,096<!--/n-->) and still falls short of linear (×<!--n scale 3proxy 1-->5.4<!--/n-->).

Validity: no client error, every best level at the proxy's CPU limit (`proxy_util` <!--n sutil-->0.98 to 1.00<!--/n-->), generator never saturated, origin at <!--n sorigin_peak-->72 %<!--/n--> of its threads at the largest point (rustle, n=8) and at most <!--n sorigin_rest-->35 %<!--/n--> at any other point. The traffic stays near <!--n gbit1kb S8.rustle-->3.6<!--/n--> Gbit/s at <!--n fmt S8.rustle ,.0f-->439,741<!--/n--> req/s, far from the <!--n c network.measured_gbit_s-->23.5<!--/n--> Gbit/s link: this curve measures the proxies, not the network.
