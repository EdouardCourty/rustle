# Results — myproxy vs 3proxy

Dates: 2026-09-27/28/29 · `{myproxy, 3proxy} × {auth=on, auth=off}`,
median of 3 runs (± warmup), local Docker client (no Internet access).

## Environment

- Dev machine: **4 GB RAM**, 2 exposed cores (auto-detected on every run — the box has
  shown "0,2", "1,3", "0-1" depending on the reboot).
- CPU pinning: **proxy → its own core**, generator/upstream/sink → the other one.
- Upstream: nginx (1kb/10kb/1mb/10mb, 300 s keep-alive). Sink: Rust discard server.
  Sysctls: `somaxconn=65535`, widened port range, `tcp_tw_reuse=1`; `ulimit -n 524288`
  everywhere.
- Raw sources: `bench/results/<proxy>/auth-<on|off>/<ts>/results.txt`.
- Memory: Rust builds are **serialized** (rustc peaks at 1-2 GB; a parallel build
  OOM-killed the box). Client alone: 53 MB at 25k tunnels.

## Final numbers (M10.3: zero-alloc parser + syscall fixes + target-cpu=native — auth on)

Protocol: randomized order with a fixed seed (`2004113323`, identical for both passes
that day), 15 s wall-clock budget, **1 KB keep-alive moved to ×64 conns** (M10.2-C: at
×16 the ceiling was the harness latency — conns/RTT — not the proxy; new protocol for
both proxies).

| Workload | **myproxy M10.3** | 3proxy | Verdict |
|---|---|---|---|
| HTTP 1 KB, keep-alive ×64 | **14,822 rps · p50 4.1 ms** | 1,555 rps · p50 41 ms | **myproxy ×9.5*** |
| HTTP 10 KB, ×16 | **13,754 rps (141 MB/s)** · CPU 12.6 s | 12,347 rps (126 MB/s) · CPU 15.3 s | **ahead: +11 % rps, +11 % MB/s, CPU −18 %** |
| HTTP 1 MB, ×16 | **2,866 rps (3,005 MB/s)** | 2,455 rps (2,574 MB/s) | **ahead: +17 %** |
| HTTP 10 MB, ×8 | **350 rps (3,668 MB/s)** | 294 rps (3,087 MB/s) | **ahead: +19 %** |
| HTTP 1 KB, 1 req/conn ×64 | **4,896 rps · p50 5.9 ms** | 3,218 rps · p50 13.3 ms | **myproxy +52 %** |
| CONNECT flood ×8 | **18.87 Gbps · CPU 4.19 s** | 17.84 Gbps · CPU 4.53 s | **ahead: +6 % throughput, CPU −8 %** |
| Hold 1,000 tunnels | **5,236 conn/s** | 4,149 conn/s | **ahead: +26 %** |
| Hold 5,000 | 956 conn/s | 955 conn/s | parity (noise) |
| Hold 10,000 | 1,741 conn/s (open 5.7 s) | 4,000 conn/s (open 2.5 s) | ⚠️ see caveat 6 (3proxy bimodal) |
| Hold 25,000 | **25,000/25,000 · rate 2,642 · CPU 3.9 s** | 16,792/25,000 (CPU 22.6 s) | **myproxy the only one to hold (CPU ÷5.8)** |
| RAM per idle tunnel | ~1.3 KB/conn | ~0.35 KB/conn | 3proxy ×4 (pooling M11) |

## CPU cost per request (new — the metric that predicts the ceiling)

The harness now publishes **all three sides**: proxy (`cpu_s_per_run`), generator
(`gen_cpu_s_per_run`) and upstream (`up_cpu_s_per_run`) — the half of the bench that had
never been published.

At the 1 KB ×64 regime (proxy 42-47 µs/req on its core, bench side ~23 µs/req — nginx
upstream ~16 + generator ~8 — on the other):

- **Proxy-alone ceiling: ~21-24k rps** (100 % of its core).
- **At 50k rps, BOTH cores saturate**: the generator+nginx side needs ~1.15 s of CPU per
  tested second at 14.8k… the 50k wall is as much the harness as the proxy (the 2nd core
  needs ~52 CPU-s over 15 s).
- M10.3 gains measured on CPU/req: 10 KB **−18 %**, 1 KB spot check **−9 %**
  (46.5 → 42.3 µs), short **−4 %**, 1 MB parity at +6.6 % rps.

## What M10.3 added (without touching the Tokio runtime)

- **Zero-alloc parser**: `RequestHead` only carries `Span`s (indices) into the read
  buffer — method/path/auth/host had been 4-6 heap allocations per request since M0;
  HTTP forwarding never copies them again (rewrite and origin key read the spans).
- **Fix: `set_nodelay` redone on EVERY REQUEST** on the pooled socket (`pipeline.rs`, run
  inside the loop) — 1 wasted syscall/req, now set once at creation.
- **Fix: empty CONNECT origin key** — all CONNECTs shared one DNS cache entry since M10.2
  (the scratch was only filled on the HTTP path); the CONNECT authority now builds its
  key.
- **TCP_QUICKACK once per connection** (instead of once per request) — the response
  piggybacks the request's ACK anyway.
- **Pool: `put` no longer allocates the key** when the origin is already pooled
  (`entry(to_string)` allocated even on a hit); auth: base64 decoding on the stack
  (`decode_slice`, allocating fallback beyond 192 B); origin key: port digits pushed
  without `to_string()`.
- **Build: `-C target-cpu=native`** in the image (rule: build where you run) — AVX2/BMI
  on this CPU class.
- **Harness: CPU of all three sides published** (`gen_cpu_s_per_run`,
  `up_cpu_s_per_run`) + 1 KB ×64 protocol (M10.2-C).
- M10.2 ×64 baseline before the fixes: 15,279 rps at 10.64 CPU-s; after: 14,822 at
  9.47-10.42 (box variance) — the CPU/req part is the lasting gain, 1 KB rps being capped
  by the harness latency.

## What M10.2 added (the last single-core pass)

- **Zero allocation on the hot path**: response buffer, rewrite buffer and origin key
  become reused connection-local scratches (~4-6 allocs/req removed, review O2).
- **TCP_QUICKACK** once per request after the head: removes the delayed-ACK window on our
  side (the same mechanism that penalizes 3proxy).
- **Verification showed no regression**: every cell stays ahead of 3proxy.

## What M9→M10.1 added (median M8 → M10.1)

1. **Relay pipes 64 KB → 1 MB** (`fcntl F_SETPIPE_SZ`, best effort, silent fallback on
   `fs.pipe-max-size`): the splice fill/drain goes from 16 iterations per MB to 1.
2. **SO_SNDBUF/SO_RCVBUF 1 MB** on every proxied socket (accept + upstream connect, via
   `setsockopt`, best effort — kernel pages are allocated on demand, an idle connection
   pays nothing).
3. **Randomized harness** (network warmup + shuffle of the 10 tests with a fixed seed):
   numbers go from ±30 % variance (flood especially) to ±3 % — medians become comparable
   across passes.
4. **Time-budget http mode** (`--duration 15`): the protocol is identical for both
   proxies (no more 17 min 1 KB run for 3proxy).
5. **Fill-to-capacity deadlock fix** (drain when loaded instead of parking on src with a
   loaded pipe): 10 MB goes from hang to 3,852 MB/s (+28 % vs 3proxy).
6. **Inactivity timeout** (IdleTimer, reset on every progress step) instead of the
   absolute timeout: real tunnels/flows longer than 60 s no longer die (B1 — critical
   before a public VPS).
7. **HTTP robustness** (code review): 1xx re-handled (100 Continue, no pool
   cross-contamination), bounded trailer (anti DoS), no pooling on backend over-read,
   idempotent-only retry (no double POST), retry extended to RST.
8. **Auth cost measured** (auth-off matrix): 1-6 % depending on the workload, invisible
   on p50 — the docs can say "auth is free".

## Auth cost (measured, auth-off vs auth-on)

| Workload | auth on | auth off | Auth overhead |
|---|---|---|---|
| HTTP 1 KB ×16 | 14,909 rps | 15,801 rps | ~5 % (within ±4 % noise) |
| HTTP 10 KB ×16 | 15,659 rps | 14,246 rps | noise (inverted) |
| HTTP 1 MB ×16 | 2,741 rps | 2,799 rps | ~2 % |
| HTTP short ×64 | 4,835 rps | 4,844 rps | ~0 % |
| 3proxy 1 KB | 388 rps | 388 rps | 0 % |

**Headline: constant-time Basic auth costs ~1-6 % CPU depending on the workload,
invisible on p50.** No reason to disable it.

## What M6-M8 changed (measured, vs M5)

1. Adaptive relay (virtual 8→64 KB buffers) then zero-copy `splice()`: tunnel ×3.8, CPU
   ÷2 to sustain it.
2. Keep-alive upstream pool (LIFO per origin, retry on a dead socket, response head
   parsing): per-request connect/close removed.
3. Idle RAM: post-hold RSS 182 → 14.3 MB at 10k tunnels (÷12).
4. DNS cache (getaddrinfo per connection, the 504 burst seen in the initial benchmark).

## Conclusions

- **myproxy is ahead EVERYWHERE on the data plane with the M10.3 protocol**: 10 KB +11 %
  (CPU −18 %), 1 MB +17 %, 10 MB +19 %, flood +6 % throughput at CPU −8 %, short +52 %,
  hold-1k +26 %, hold-25k 25,000/25,000 (CPU ÷5.8).
- **1 KB ×64 is a harness latency wall** (~15-16k whatever the fastest proxy) — the
  honest metric for the next optimizations is **CPU per request**, now published.
- **The 50k wall is DOUBLE on this box**: proxy ~21-24k (CPU/req) AND bench side
  ~23 µs/req — not "physically impossible", but two core budgets to divide by ~3 each
  (proxy: io_uring/FSM without Tokio; harness: a lighter upstream than nginx).
- **Connection setup + tunnels: myproxy dominates** (×36 on small objects, +62 % on short
  connections, and the only one to hold 25,000 concurrent tunnels on this machine).
- **Reliability note**: inter-pass variance stays ±3-5 % on the box (2 shared cores,
  neighbouring containers). Deltas ≥ +5 % are above noise; micro-gaps are not significant.
- **Constant-time auth is free** as far as measurement goes (~1-6 % depending on the
  workload, invisible on p50).
- **Idle RAM**: ×4 vs 3proxy — Tokio's task-per-conn architecture costs ~1.1-1.4 KB/conn
  versus ~0.35 (task pooling = M10).

## Honest caveats

1. 3proxy's 1,555 req/s on HTTP 1 KB ×64 = **delayed ACK (40 ms) verified in a capture**:
   3proxy answers in 2 small writes, the 2nd waits for a delayed ACK of exactly 40 ms (p50
   41 ms, identical to the ×16 protocol). This ×9.5 is real here but is above all a
   3proxy defect (fixable on their side, TCP_NODELAY) — never publish it without this
   caveat.
2. **Flood variance up/down depending on warmup**: 17.2 Gbps after 4 heavy HTTP tests,
   24.3 Gbps in isolation. The harness needs a randomized order or a pre-warmed network
   init to refine.
3. 3proxy at 25k: accepts then closes without answering beyond ~17k tunnels (best-effort
   config `maxconn 200000`).
4. Client + sink on the same core: the harness's absolute ceiling for the flood.
5. RAM measured after the test, identical modes for both proxies.
6. **hold-10k bimodal on the 3proxy side**: 1,672-1,877 conn/s on previous days' passes,
   4,000 conn/s today (same protocol, same seed) — mass connection setup depends heavily
   on network state (conntrack/TIME_WAIT inherited from the position in the matrix). The
   reliable readings: hold-25k (the structural wall) and hold-1k (pure setup);
   hold-5k/10k remain accept-bandwidth tests.

## M10 backlog (driven by these numbers)

1. **Stabilize harness variance** (randomized test order, pre-warmed network init, 2
   shared cores = variable ceiling): the last % (isolated 10 MB +24 % vs matrix −2 %) get
   lost in the noise.
2. **Tokio task/buffer pooling**: RAM ×3-4 vs 3proxy → ~0.3 KB/conn.
3. **The VPS**: the 50k/100k hold matrix + real scaling. Single worker tuned to the max
   here (assumed constraint: no multi-worker); the "4 GB / 2 cores" constraint of the dev
   box is no longer the limiter once on the VPS.

## M10.4 (back to Tokio, verified gains reapplied) — 2026-09-29, seed 2004113323

**Decision**: the M11 pass (epoll FSM without Tokio) is kept under the `m11.1-fsm` tag and
the production line went back to Tokio (`m10.3-tokio`). The data decided: Tokio CPU/request
≈ FSM (46.5 vs 47 µs) but Tokio holds the best rps on the HTTP path (1kb 14.8k vs 11.5k),
its splice body rail works, and the hand-rolled architecture cost ~800 lines of in-house
concurrency + 4 serious bugs found in a single session (silent spin without a syscall, a
fixed budget compared with a counter, etc.). The container's syscall profile (strace
debug image) confirms that **M10.3 is already at its architecture's floor: ~2 recvfrom + 2
sendto per request, epoll_ctl ≈ 0**.

**M10.4 = tag M10.3 +**:
1. `err_kinds` in the generator (taxonomy eof-head/bad-status-\<code\>/eof-body/bad-head/io/connect,
   published in the metric — the tool that settled every bug of the M11 pass).
2. Upstream `writev`: head + body prefix in **one** syscall (`write_vectored` + advance
   loop; the pipelined-body check moved before the write = no more partial write).
   Neutral on GETs (1-write fast path already), real gains on uploads.
3. `BENCH_ONLY=<test>`: validate/re-run a single step of the matrix (a faulty step is
   replayed alone, not the whole matrix).
4. `bench/tools/local-upstream-http11.py`: local HTTP/1.1 keep-alive upstream — it
   uncovered the measurement trap (an HTTP/1.0 upstream legitimately reconnects on every
   request; the "hot path" syscall profile was actually measuring the fresh-connect path).

**M10.4 matrix vs fresh tag baseline vs frozen M10.3 matrix** (seed 2004113323):
1kb 14,693 (+3.4 %) · 10kb 13,792 (+4.0 %) · 1mb 2,597 (−5.2 %) · 10mb 360 (+2.3 %) ·
short 4,750 (+39 %, low baseline: back to the frozen 4,896 level) · flood 19 Gbps (−1.9 %) ·
holds ±8 % (known variance). All err_kinds empty. **writev is neutral on the matrix as
predicted (GET = fast path) and correct for uploads.**

**Caveats kept**: ×9.5 at 1kb vs 3proxy (delayed ACK, tcpdump-verified), bimodal hold
variance (above).

**Next backlog**: port of the buffered chunked relay (M10.3 reads the framing byte by byte,
~1 syscall/byte — outside the matrix, real outside the bench), VPS profiling, PGO.

## M10.5 (fixes + single-core squeeze) — 2026-09-30, seed 2004113323, auth on

**Fixes** (outside the matrix: the bench serves Content-Length): chunked relay rewritten
(bytes read along with the head were forwarded raw, then the parser started out of sync →
client connection blocked until the idle timeout; staging = `resp_buf`, budget enforced
while relaying, bounded trailers), requests with an invalid or contradictory TE /
Content-Length → 400 (anti smuggling), backoff on `accept` errors, `config.toml.example`
(duplicated `[http]` table), pool test (tokio 1.53 refuses a blocking fd).

**Measurements, one matrix per step** (µs proxy CPU/req; noise seen between two matrices
with identical code on these paths: up to ±7 % CPU, ±13 % 10mb throughput):

| Step | Measured effect |
|---|---|
| S1 `current_thread` runtime (workers=1) | short −11 % CPU/req; the rest within noise |
| S2 response head read window 2 → 16 KB | 10kb: 2 recv+2 send → 1+1 (strace), CPU within noise |
| S3 no more allocation per response (`to_ascii_lowercase`, 8 KB Vec when the body is already read) | noise |
| S4 NODELAY/SNDBUF/RCVBUF set on the listener (inherited, tested) | short a further −2.4 % |
| mimalloc (retest) | noise (allocation-free hot path) — not kept |

**Final vs HEAD M10.4-2**: short 101.2 → 88.8 µs/req (**−12 %**, stable over 5 matrices),
10kb −5 %, 10mb −5 %, holds CPU −2 to −7 %, 1kb neutral. Throughput unchanged: the
harness (2 cores) caps it.

**User/sys split (1kb ×64)**: user ≈ 9 µs/req, sys ≈ 35 µs/req. ~80 % of the proxy CPU is
the loopback TCP stack (which includes the peers' RX processing); syscalls are at the floor
(2 recv + 2 send + epoll_wait). Rust code can only win on ~20 % of the CPU → PGO, timer
rework, FxHash: expected gains < noise, not done.

**Memory — measurement bias fixed**: `rss_mb` is read *after* the test ends (retained
memory), not during it. New `peak_rss_mb` (VmHWM): **~5 KB per live tunnel** (hold-25k:
123-125 MB, identical HEAD and final) — the "~1.3 KB per tunnel" published above was the
residual. With `current_thread`, everything lives in glibc's main arena (never shrunk):
125 MB residual. Fix: `malloc_trim` when connections fall under half of the peak (5 s
tick) → 2 MB residual.

## M13 — 5 proxies × 3 transports campaign — 2026-09-30, seed 2004113323, auth on, 1 proxy core

Protocol: the usual matrix (REPEAT=3, median), the proxy alone on 1 core, the harness on
the other. Cell = throughput · proxy CPU per request (container cgroup). Peak RAM =
`peak_rss_mb` (sum over processes). DUTs: 3proxy 0.9.4, Squid 5.7 (cache off,
`access_log none`), gost 3.2.6 (access logs off: it logged every connection at `info`, a
cost the others do not pay), Tinyproxy 1.11. TLS: an ECDSA P-256 certificate of the same
kind for everyone. 0 HTTP errors everywhere.

### Plain (HTTP/1.1)

| Test | **myproxy** | 3proxy | Squid | gost | Tinyproxy |
|---|---|---|---|---|---|
| 1 KB ×64 | **14,291** · 49 µs | 1,555¹ · 75 µs | 9,113 · 120 µs | 1,913 · 570 µs | 2,806 · 347 µs |
| 10 KB ×16 | **13,532** · 62 µs | 12,475 · 80 µs | 7,308 · 149 µs | 1,847 · 580 µs | 2,394 · 411 µs |
| 1 MB ×16 | **2,642** · 334 µs | 2,146 · 395 µs | 309 · 3,574 µs | 695 · 1,563 µs | 129 · 8,566 µs |
| 10 MB ×8 | **375** · 2.3 ms | 281 · 2.7 ms | 32 · 35 ms | 104 · 10.6 ms | 14 · 81 ms |
| 1 KB short | **4,805** · 88 µs | 3,407 · 260 µs | 3,794 · 235 µs | 1,880 · 573 µs | 2,409 · 366 µs |
| flood | **19.2 Gbps** | 18.0 | 12.8 | 11.7 | 1.2 |
| hold-10k (peak RAM) | 10,000 · **52 MB** | 10,000 · 231 MB | 10,000 · 450 MB | 10,000 · 1,352 MB | 10,000 · 189 MB |
| hold-25k | **25,000** · 119 MB | 17,146 · 371 MB | 25,000 · 882 MB | 14,618² | 16,746 · 314 MB |

### TLS (HTTPS proxy, HTTP/1.1)

| Test | **myproxy** | Squid | gost |
|---|---|---|---|
| 1 KB ×64 | **14,122** · 57 µs | 7,790 · 140 µs | 1,614 · 656 µs |
| 10 KB ×16 | **13,375** · 62 µs | 6,089 · 180 µs | 1,539 · 685 µs |
| 1 MB ×16 | **1,285** · 819 µs | 177 · 6,218 µs | 464 · 2,327 µs |
| 10 MB ×8 | **105** · 10.3 ms | 18 · 61 ms | 60 · 18.2 ms |
| 1 KB short (handshake/req) | **2,726** · 269 µs | 1,605 · 677 µs | 1,599 · 669 µs |
| flood | **9.2 Gbps** | 4.9 | 6.7 |
| hold-10k (peak RAM) | 10,000 · **332 MB** | 10,000 · 803 MB | 10,000 · 1,736 MB |
| hold-25k | **25,000** · 659 MB | 25,000 · 1,774 MB | 21,924² |

### HTTP/2 (TLS, ALPN h2; 1 h2 connection per generator "conn")

| Test | **myproxy** | gost |
|---|---|---|
| 1 KB ×64 | **12,975** · 70 µs | 3,934 · 280 µs |
| 10 KB ×16 | **11,958** · 83 µs | 3,676 · 298 µs |
| 1 MB ×16 | **726** · 1.4 ms | 323 · 3.4 ms |
| 10 MB ×8 | **69** · 15 ms | 35 · 31 ms |
| 1 KB short | **2,240** · 349 µs | 1,469 · 680 µs |
| flood (CONNECT on a stream) | 6.6 Gbps · 1,078 ms/GB | **7.7 Gbps** · 1,106 ms/GB |
| hold-10k (peak RAM) | 10,000 · **622 MB** | 10,000 · 1,990 MB |
| hold-25k | 21,351³ · 882 MB | 743² |

¹ 3proxy's 40 ms delayed ACK (historical caveat). ² container died during the test (no
more measurable CPU, peak 0): the box's RAM (4 GB) was exceeded — gost ≈ 135-175 KB/tunnel.
³ limited by the **generator** (one TLS+h2 connection per tunnel: 39 s CPU on its core,
868 MB, 145 s to open; myproxy 21 s CPU) — inconclusive on this box.

**Reading**: myproxy is first on every plain and TLS row, with the lowest CPU/req and the
lowest RAM. Over TLS, small keep-alive requests cost almost nothing (1 KB +15 % CPU/req,
10 KB ≈ 0); bulk pays for encryption and the loss of splice (1 MB ×2.5, 10 MB ×4.4 CPU/req,
flood ×5.6 CPU/GB) — the kTLS target (M13.3). In h2: ahead of gost everywhere except the
flood (−14 % throughput, equivalent CPU/GB); the h2 path is slower than TLS+HTTP/1.1 on
bulk (1 MB 726 vs 1,285): h2 framing + a `Bytes` copy per chunk — an optimization lead.

**Caveats**: 2-core box, loopback, one seed; multi-core not measured here (VPS:
`run-scaling.sh`). TLS tunnels: ~26 KB/tunnel for myproxy (rustls buffers) versus ~5 KB
plain.

## M16 — access log cost — 2026-09-30, seed 2004113323, auth on, 1 core

| µs proxy CPU/req | log `off` | `all` v1 (1 writer wakeup/line) | `all` batched (per-shard batch) |
|---|---|---|---|
| 1 KB ×64 | 49.3 | 62.2 (+26 %) | **50.7 (+2.9 %)** |
| 1 KB short | 90.0 | 110.2 (+22 %) | **91.0 (+1.1 %)** |
| 10 KB / 1 MB / 10 MB / flood | — | +7 to +13 % | within noise |

v1 sent every line to the writer thread through the channel: one wakeup (futex + context
switch on the same core) per request cost more than all the user-space processing
(~9 µs). Kept version: lines accumulated in a buffer local to the shard thread (lock-free),
handed to the writer in 64 KB batches or every 200 ms. `off`: no regression vs M15.

## M17 — kTLS (kernel TLS) — 2026-10-01, seed 2004113323, auth on, 1 core, HTTPS proxy

rustls does the handshake, the keys go to the kernel (`TCP_ULP tls` + `SOL_TLS`), splice
comes back on the TLS port. Tested for real: TLS 1.3, TLS 1.2 AES-128/256-GCM and
ChaCha20, 10 MB through GET and through a CONNECT tunnel identical to a direct download.

| µs proxy CPU/req (throughput) | userspace rustls | kTLS | kTLS + TX_ZEROCOPY_RO / RX_NO_PAD |
|---|---|---|---|
| 1 KB | 56.8 | 60.1 (+6 %) | — |
| 10 KB | 62.5 | 67.5 (+8 %) | — |
| 1 MB | 819 (1,287 rps) | 1,010 (815 rps, **−37 %**) | 1,126 (813 rps) |
| 10 MB | 10,033 (108 rps) | 11,796 (86 rps) | 10,378 (93 rps) |
| 1 KB short (handshake/req) | 276 | 326 (+18 %) | 319 |
| flood (CONNECT) | 945 ms/GB (9.2 Gbps) | **824 ms/GB (10.4 Gbps, +13 %)** | 826 (10.3 Gbps) |
| holds | — | +14 to +18 % CPU | — |
| hold-25k peak RAM | 814 MB | **569 MB (−30 %)** | — |

h2 over kTLS: −40 % on 1 MB / 10 MB. **Decision**: `tls.ktls = false` by default, a
documented option — a win for CONNECT tunnels (HTTPS browsing) and memory, a loss on HTTP
bulk and per connection (setup: rustls drain + ULP + keys).

## M18 — io_uring, prototype measured before any migration — 2026-10-01, seed 2004113323

monoio prototype (`bench/uring-proto`, DUT `uring-proto`; since removed from the repo,
recoverable at commit `a2b11e3`) of the HTTP/1 keep-alive path (same rewrite, auth, kept
upstream, Content-Length responses, per-operation timeout). Compared with myproxy with
seccomp lifted for both (`SECCOMP=unconfined`: Docker's profile blocks io_uring and the
filter costs per syscall). Alternating runs:

| µs proxy CPU/req | io_uring (proto) | Tokio (myproxy) |
|---|---|---|
| 1 KB ×64 | 53.7 | **49.8** |
| 10 KB ×16 (2 runs) | **57.1 / 57.5** | 62.4 / 62.0 |

Without timeouts the prototype looked −10 to −14 % on 10 KB: part of it came from the
absence of timers, not from io_uring. 1 MB / 10 MB / short not comparable (prototype
without splice or shared pool). **Verdict**: ±8 % depending on size, no net gain — the
same conclusion as M11 (epoll FSM): on one core, the cost is in the kernel TCP stack. No
migration; to be retested on the multi-core VPS (submission batching).

## M19 — final campaign — 2026-10-01, seed 2004113323, auth on, 1 proxy core

`bench/run-campaign.sh` (5 proxies × every supported transport), tables by
`bench/tools/campaign-table.py`. Cell = throughput · proxy CPU per request (http: µs/req,
flood: ms/GB); holds = tunnels open / requested · peak RAM. Default myproxy configuration
(kTLS off, log off). No errors (`err_kinds` empty); only Tinyproxy's reconnects (1 per
request, it closes without announcing it).

### plain

| test | myproxy | 3proxy | squid | gost | tinyproxy |
|---|---|---|---|---|---|
| http-1kb | 14,332 rps 49µs | 1,558 rps 74µs | 8,841 rps 124µs | 1,930 rps 561µs | 2,627 rps 366µs |
| http-10kb | 14,024 rps 61µs | 13,207 rps 79µs | 7,174 rps 152µs | 1,864 rps 575µs | 2,606 rps 396µs |
| http-1mb | 2,622 rps 336µs | 2,437 rps 369µs | 306 rps 3601µs | 703 rps 1553µs | 130 rps 8445µs |
| http-10mb | 347 rps 2168µs | 279 rps 2628µs | 32 rps 35203µs | 104 rps 10562µs | 14 rps 82017µs |
| http-1kb-short | 4,822 rps 87µs | 3,345 rps 261µs | 3,775 rps 237µs | 1,927 rps 567µs | 2,802 rps 356µs |
| flood | 19.0 Gbps 152ms/GB | 17.7 Gbps 192ms/GB | 12.9 Gbps 674ms/GB | 11.6 Gbps 750ms/GB | 1.2 Gbps 6913ms/GB |
| hold-1000 | 1,000/1000 10.2MB | 1,000/1000 26.0MB | 1,000/1000 164.8MB | 1,000/1000 179.9MB | 1,000/1000 21.8MB |
| hold-5000 | 5,000/5000 29.8MB | 5,000/5000 117.1MB | 5,000/5000 291.7MB | 5,000/5000 774.5MB | 5,000/5000 95.9MB |
| hold-10000 | 10,000/10000 52.2MB | 10,000/10000 230.8MB | 10,000/10000 449.7MB | 10,000/10000 1446.2MB | 10,000/10000 189.6MB |
| hold-25000 | 25,000/25000 124.8MB | 24,959/25000 384.5MB | 25,000/25000 875.3MB | 18,656/25000 0.00MB | 16,765/25000 314.0MB |

### tls

| test | myproxy | squid | gost |
|---|---|---|---|
| http-1kb | 14,018 rps 57µs | 7,837 rps 141µs | 1,566 rps 676µs |
| http-10kb | 13,540 rps 62µs | 5,999 rps 183µs | 1,550 rps 690µs |
| http-1mb | 1,297 rps 813µs | 182 rps 6060µs | 464 rps 2328µs |
| http-10mb | 108 rps 10017µs | 18 rps 60648µs | 59 rps 18334µs |
| http-1kb-short | 2,722 rps 278µs | 1,618 rps 669µs | 1,603 rps 661µs |
| flood | 9.3 Gbps 938ms/GB | 4.8 Gbps 1796ms/GB | 6.6 Gbps 1309ms/GB |
| hold-1000 | 1,000/1000 47.9MB | 1,000/1000 203.5MB | 1,000/1000 246.5MB |
| hold-5000 | 5,000/5000 222.8MB | 5,000/5000 462.9MB | 5,000/5000 1044.2MB |
| hold-10000 | 10,000/10000 438.7MB | 10,000/10000 811.6MB | 10,000/10000 1863.8MB |
| hold-25000 | 25,000/25000 802.4MB | 25,000/25000 2028.0MB | 0/25000 0.00MB |

### h2

| test | myproxy | gost |
|---|---|---|
| http-1kb | 13,028 rps 70µs | 3,922 rps 281µs |
| http-10kb | 12,232 rps 82µs | 3,709 rps 295µs |
| http-1mb | 739 rps 1428µs | 324 rps 3374µs |
| http-10mb | 68 rps 15495µs | 35 rps 30183µs |
| http-1kb-short | 2,281 rps 354µs | 1,432 rps 689µs |
| flood | 6.6 Gbps 1074ms/GB | 7.7 Gbps 1105ms/GB |
| hold-1000 | 1,000/1000 93.2MB | 1,000/1000 259.0MB |
| hold-5000 | 5,000/5000 462.9MB | 5,000/5000 997.1MB |
| hold-10000 | 10,000/10000 807.2MB | 10,000/10000 0.00MB |
| hold-25000 | 21,291/25000 1112.7MB | 931/25000 0.00MB |

Notes: 3proxy 1 KB = 40 ms delayed ACK (historical caveat); 3proxy holds 24,959 / 25,000
tunnels this time (17,146 in M13 — known variance of its wall at ~17-25k); gost: container
killed for lack of RAM at 25k (plain and TLS) and right after hold-10k in h2 (peak 0);
myproxy h2 hold-25k limited by the generator (one TLS+h2 connection per tunnel), as in
M13. Gaps to M13 within noise.

## M20 — HTTP/2 tunnels: catch up with, then pass gost — 2026-10-02, seed 2004113323

Finding (M19): h2 flood 6.6 Gbps vs gost 7.7, at equal CPU/GB — myproxy used only 87 % of
its core (gost 104 %): it was waiting. Step-by-step measurements (flood-8, PROXY_H2=1):

| Step | Throughput | Proxy CPU | Reading |
|---|---|---|---|
| start | 6.6 Gbps | 87 % | flow control: the client waits for WINDOW_UPDATEs |
| A. window reopened on receipt (not after the upstream write) | 6.6 | 84 % | no effect alone, kept (sound, bounded by the window) |
| B. windows 1 → 4 MB per stream, 8 → 16 MB per connection | **7.86** | 101 % | bottleneck lifted; the generator spends 7.7 → 6.2 s CPU |
| C. read-ahead under rustls (rustls reads 4 KB at a time: ~4 recv per 16 KB record; adaptive 64 KB buffer, released when the flow becomes light again) | ≈ | −3 % CPU/GB | h2 bound by the harness core; HTTP/1 over TLS: flood 9.3 → 9.6 Gbps, −5 % CPU/GB |

Final alternating A/B (3 passes each): **myproxy 7.64 / 7.54 / 7.58 Gbps (0.97 s CPU/GB)
vs gost 7.40 / 7.41 / 7.55 (1.08 s/GB)** — ahead in every pair, −11 % CPU per GB. h2
throughput is capped by the harness core (generator + sink): the throughput gap stays
narrow, the efficiency gap is clear. Full TLS and h2 matrices: no regression (every other
test within noise), peak RAM unchanged (windows reserve nothing upfront), `err_kinds`
empty.


## M21 — optimization pass from two research tracks (other proxies + overall view) — 2026-10-02/03, seed 2004113323, auth on, 1 core

Method: every lead is an **alternating** A/B (base / variant / base / variant…, image
rebuilt on every switch, proxy restarted before every test), 2 to 3 passes, kept only
above noise. Metric: proxy CPU per request (or per GB for floods).

**Protocol change (M21.5)**: the warmup now runs in a separate generator run before
sampling. Its CPU used to be amortized into every number (~+6 to +8 %: 1 KB 49 → 45
µs/req). **Do not compare M21+ numbers with earlier campaigns.** The harness also records
the proxy netns TCP counters per run: `segs_out_per_run`, `active_opens_per_run`
(upstream pool churn), `retrans_per_run`, `delayed_acks_per_run`, `listen_overflows`,
`listen_drops`. New test outside the matrix: `flood-128` (`BENCH_ONLY=flood-128`).

| Step | Change | Result |
|---|---|---|
| M21.1 | Splice engine redone: per-shard pipe pool (16 max, returned when empty), pipes < 64 KB refused (the `fs.pipe-user-pages-soft` budget shrinks pipes to 8 KB beyond ~60: 256 splices/MB), splice via `try_io` on the Tokio sockets (no more dup or epoll re-registration), lazy tunnels (adaptive copy, then a pipe when sustained) | flood-128 **10.0 → 16.8–17.0 Gbps** (3proxy 10.9), CPU/GB 0.86 → 0.38; 1 MB −8 %; holds −10 to −14 % CPU; hold-25k peak RAM 125 → 121 MB |
| M21.2 | Splice from the first byte when the declared remaining body is ≥ 128 KB | 1 MB 300 → 253 µs/req (−16 %), 10 MB −4.5 % |
| M21.3 | TLS: AES-128-GCM first, server order wins (sessions were running AES-256) | TLS 1 MB −7 % CPU/req (+7 % rps), TLS flood −4 % CPU/GB, h2 flood −6 % CPU/GB |
| M21.4 | h2: copy-free response bodies per frame (`split_to().freeze()` on `BytesMut`) | h2 1 MB −5.7 % CPU/req (+4.5 % rps), 10 MB −2.7 % |
| M21.5 | Harness (see above) | finding: 0.0116 upstream opens/req at 1 KB ×64 |
| M21.6 | Upstream pool `max_idle_per_origin` 32 → 256 (64 clients > 32 slots: ~2,500 upstream reconnects per run); emptied origin entry kept | upstream opens/req → 0, 1 KB −2.7 % CPU/req |
| M21.7 | `TCP_DEFER_ACCEPT` on the listeners (every served protocol is client-speaks-first); timers: `biased;`, redundant outer timeout removed | short 79.8 → 73.9 µs/req (**−7.4 %**); timers alone neutral |
| M21.8 | Multi-core: AuthLimiter (bug: a failing IP was never forgotten → global lock on every accept), DNS 1 lock, per-shard counters on their own cache line, `ConnSlot` on the shard's `Arc`, stateless TLS tickets | not measurable on 1 core; non-regression OK (noise) |
| M21.9 | `server.socket_buffer_bytes` (default 1 MB, 0 = autotuning) | see below |
| M21.10 | `serve`'s keep-alive state boxed (connection future 3,760 → < 2,560 bytes: a held tunnel kept the size of the largest branch) | hold-25k peak RAM 118.5/120.6 → **81.7/79.4 MB (−33 %)**, hold-10k 51.4/51.7 → 36.1/35.3 MB (−31 %); CPU, 1 KB and short within noise (1 malloc per connection) |
| M21.11 | TLS copy buffer (non-spliceable client side) max 64 → 256 KB, adaptive growth unchanged | TLS flood 10.26/10.29 → **11.38/11.46 Gbps** (CPU/GB 0.78 → 0.69, −11 %), TLS 10 MB 8,329/8,512 → 7,633/7,740 µs/req (−9 %), TLS 1 MB −3.6 % (noise) |
| M21.12 | CPU steering (`SO_ATTACH_REUSEPORT_CBPF`) when `pin_cores` gives one shard per CPU: the connection goes to the shard pinned on the CPU that processed its SYN | not measurable on 1 core; verified: client pinned on CPU 1 → 40/40 connections on shard 1 |

**Socket buffers: fixed 1 MB vs kernel autotuning** (A/B ×2, bench = Docker LAN):

| Test | 1 MB | autotuning |
|---|---|---|
| 1 KB | 44.1 / 44.6 µs/req | 48.5 / 49.3 |
| 1 MB | 231 / 223 µs/req | 248 / 238 |
| 10 MB | 319 / 362 rps | 247 / 249 |
| flood-8 | 19.2 / 19.3 Gbps, 0.14 s CPU/GB | 17.1 / 17.2 Gbps, 0.32 |

1 MB default kept. But a fixed size disables autotuning: at most 1 MB in flight per
connection (~80 Mbit/s at 100 ms RTT), and it is capped by `net.core.rmem_max`/`wmem_max`
(208 KB on a stock kernel → ~416 KB effective, ~33 Mbit/s at 100 ms; warning at startup).
For distant clients: `socket_buffer_bytes = 0`.

**Rejected (measured neutral)**: QUICKACK 1×/connection vs none (1 KB, short, 10 KB within
noise: kept as is); QUICKACK=0 on the listener (HAProxy's "smart accept") not tested:
`inet_csk_clone_lock` resets the ACK state on the accepted socket, nothing is inherited.

### M21 final campaign — 2026-10-03, seed 2004113323, auth on, 1 proxy core (protocol M21.5)

Index: `results/campaign-20261003-015025.txt`. Throughput · proxy CPU per request (ms/GB
for floods); holds = open/requested · peak RAM. `err_kinds` empty everywhere on the proxy
side. h2 hold-25000: 21,264/25,000 (harness ceiling, stable since the first campaign:
21,351 on 09/30; gost 462), peak RAM 1,113 → 941 MB since M19. h2 flood: 7.4 vs 7.6 Gbps
for gost (flood noise ±15 %), 0.91 vs 1.05 s CPU/GB.

#### plain

| test | myproxy | 3proxy | squid | gost | tinyproxy |
|---|---|---|---|---|---|
| http-1kb | 14,594 rps 44µs | 1,557 rps 67µs | 8,964 rps 110µs | 1,935 rps 510µs | 2,962 rps 313µs |
| http-10kb | 14,211 rps 54µs | 12,343 rps 74µs | 7,292 rps 136µs | 1,850 rps 521µs | 2,601 rps 353µs |
| http-1mb | 2,453 rps 224µs | 2,159 rps 329µs | 306 rps 3247µs | 688 rps 1426µs | 128 rps 7783µs |
| http-10mb | 319 rps 1811µs | 264 rps 2214µs | 31 rps 31794µs | 104 rps 9518µs | 14 rps 73734µs |
| http-1kb-short | 4,884 rps 75µs | 3,403 rps 230µs | 3,795 rps 212µs | 1,916 rps 507µs | 2,996 rps 306µs |
| flood | 19.3 Gbps 153ms/GB | 18.4 Gbps 197ms/GB | 13.0 Gbps 626ms/GB | 11.6 Gbps 701ms/GB | 1.2 Gbps 6541ms/GB |
| hold-1000 | 1,000/1000 9.2MB | 1,000/1000 26.1MB | 1,000/1000 164.7MB | 1,000/1000 179.2MB | 1,000/1000 21.7MB |
| hold-5000 | 5,000/5000 20.1MB | 5,000/5000 117.1MB | 5,000/5000 291.2MB | 5,000/5000 758.8MB | 5,000/5000 95.9MB |
| hold-10000 | 10,000/10000 35.9MB | 10,000/10000 230.9MB | 10,000/10000 449.8MB | 10,000/10000 1526.1MB | 10,000/10000 188.6MB |
| hold-25000 | 25,000/25000 78.0MB | 17,154/25000 380.3MB | 25,000/25000 896.1MB | — | 16,746/25000 314.0MB |

#### tls

| test | myproxy | squid | gost |
|---|---|---|---|
| http-1kb | 14,128 rps 50µs | 7,826 rps 126µs | 1,611 rps 601µs |
| http-10kb | 12,895 rps 59µs | 5,993 rps 165µs | 1,536 rps 615µs |
| http-1mb | 1,444 rps 654µs | 178 rps 5583µs | 468 rps 2069µs |
| http-10mb | 126 rps 7842µs | 18 rps 54105µs | 60 rps 16442µs |
| http-1kb-short | 2,727 rps 233µs | 1,600 rps 611µs | 1,566 rps 605µs |
| flood | 11.4 Gbps 710ms/GB | 4.8 Gbps 1680ms/GB | 6.7 Gbps 1214ms/GB |
| hold-1000 | 1,000/1000 47.7MB | 1,000/1000 201.6MB | 1,000/1000 233.0MB |
| hold-5000 | 5,000/5000 221.0MB | 5,000/5000 464.4MB | 5,000/5000 952.6MB |
| hold-10000 | 10,000/10000 437.7MB | 10,000/10000 829.7MB | 10,000/10000 2036.2MB |
| hold-25000 | 25,000/25000 787.6MB | 25,000/25000 1845.9MB | 1,226/25000 0.00MB |

#### h2

| test | myproxy | gost |
|---|---|---|
| http-1kb | 14,896 rps 57µs | 3,904 rps 254µs |
| http-10kb | 11,924 rps 76µs | 3,700 rps 266µs |
| http-1mb | 792 rps 1190µs | 317 rps 3093µs |
| http-10mb | 75 rps 12692µs | 35 rps 27747µs |
| http-1kb-short | 2,324 rps 315µs | 1,464 rps 620µs |
| flood | 7.4 Gbps 910ms/GB | 7.6 Gbps 1046ms/GB |
| hold-1000 | 1,000/1000 94.1MB | 1,000/1000 263.5MB |
| hold-5000 | 5,000/5000 465.4MB | 5,000/5000 1137.4MB |
| hold-10000 | 10,000/10000 786.1MB | 10,000/10000 1899.1MB |
| hold-25000 | 21,264/25000 940.9MB | 462/25000 0.00MB |

## M22 — black-box e2e suite and the fixes it revealed — 2026-10-03, seed 2004113323, auth on, 1 core

New `e2e/` suite (see `e2e/README.md`): the release binary, one process per scenario,
real clients (HTTP/1.1, HTTPS, h2, SOCKS5 TCP/UDP, curl) and programmable origins. 527
scenarios, ~2 min. Result: 524 OK, 3 SKIP (curl 7.88 without `--proxy-http2`), 3
consecutive runs without a flake. The quick subset (62 scenarios, ~20 s) is part of
`make check`.

Proxy defects found and fixed:

| # | Case | Before | After |
|---|---|---|---|
| 1 | Origin closing without answering / silent (HTTP/1) | bare close (the log said 502/504) | 502 / 504 |
| 2 | Silent origin (h2) | 502 | 504 |
| 3 | 1xx and final response in the same segment | final response lost | relayed |
| 4 | EOF-delimited response | client kept in keep-alive, stuck until `idle` (60 s) | close after the response |
| 5 | POST on a closed pooled socket (HTTP/1); non-idempotent request without body (h2) | **replayed** (double execution, invariant B7) | 502, never replayed |
| 6 | Write failure to a dead pooled socket | bare close | retry when replayable, 502 otherwise |
| 7 | Pipelined requests | close without a response | 1st response, then close |
| 8 | Chunked upload over budget / invalid framing | bare close | 413 / 400 |
| 9 | Upload cut by the client | origin waiting until its timeout | immediate abort |
| 10 | CONNECT tunnel over kTLS: end of the upstream flow | FIN without `close_notify` | `close_notify`, then FIN |
| 11 | Splice: reset or kTLS `close_notify` on the source side | pipe not drained, half-close lost (tunnel stuck until `idle`) | handled as EOF |
| 12 | SIGTERM / SIGHUP right after startup | process killed (handlers installed after the listeners) | handlers installed first |
| 13 | stderr logs outside a terminal | ANSI color codes | plain text |

Verified mutations (the suite must fail): splice half-close disabled (2 failures in
quick), `Proxy-Authorization` forwarded to the origin (5 failures in quick),
`allowed_ports` ignored (6 failures in the full suite; since then the HTTP/1 and SOCKS5
policy cases are in quick).

Non-regression: alternating A/B ×2, base (M21) vs M22, seed 2004113323.

| Test | base | M22 |
|---|---|---|
| 1 KB µs CPU/req | 45.0 / 44.1 | 43.9 / 42.7 |
| short µs CPU/req | 75.0 / 74.0 | 73.3 / 75.9 |
| 1 MB µs CPU/req | 241.1 / 218.5 | 227.4 / 222.7 |
| flood-8 Gbps (s CPU/GB) | 19.03 (0.195) / 18.73 (0.165) | 19.37 (0.159) / 18.95 (0.150) |
| flood-8 TLS Gbps (s CPU/GB) | 11.44 (0.708) / 11.44 (0.706) | 11.47 (0.699) / 11.48 (0.702) |

Everything is within noise: no regression.
