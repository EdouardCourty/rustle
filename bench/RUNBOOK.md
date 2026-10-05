# Final benchmark: runbook

Two phases, because a cloud NIC caps tunnel throughput and a Mac does not:

| Phase | Where | What it measures | Result goes to |
|---|---|---|---|
| **A** | three machines (generator → proxy → origin), private network | requests/s, latency, TLS (resumed and full handshake), WebSocket, idle tunnels (RAM), CPU per request, and the multi-core curve | the README table and charts |
| **B** | this Mac (Docker, one host, loopback) | tunnel throughput (`flood`): no NIC in the way, the proxy and the kernel are the limit | the "Tunnels" row and chart (`FLOOD_FROM`) |

The README says which environment each number comes from.

## 0. Before anything

* `git status` clean and pushed, `make check` green. Every machine runs the same commit (preflight checks it).
* Read `FAIRNESS.md`: versions and settings of the competitors, and what is deliberately not tuned.
* Commit nothing and start no other workload on the machines during a run. On the Mac, quit every other container and heavy app (the numbers are only as quiet as the box).

## Phase A: three machines

### 1. Machines

Scaleway Elastic Metal (hourly billing, never monthly), same zone, private network option on all three. See `REMOTE.md` "Renting the machines". Suggested: proxy EM-I120E (8C/16T, 25 Gbit/s) or EM-B230E (10 Gbit/s); generator EM-B330E (12C/24T); origin EM-B130E. Ubuntu 24.04 on each, your SSH key.

### 2. Bootstrap (once per machine)

```sh
for h in $GEN_HOST $PROXY_HOST $UP_HOST; do
  ssh root@$h 'bash -s' < bench/remote/bootstrap.sh
  ssh root@$h 'git clone https://github.com/EdouardCourty/rustle ~/rustle'   # or git pull
done
```

### 3. Settings used for every command below

```sh
export GEN_HOST=...   PROXY_HOST=...   UP_HOST=...        # ssh targets
export GEN_ADDR=10.x  PROXY_ADDR=10.y  UP_ADDR=10.z        # private addresses
export PROXY_CPUS=0 WORKERS=1                              # one core for every proxy
```

### 4. Preflight (always, before a campaign)

```sh
APPLY=1 ./bench/preflight.sh          # moves NIC interrupts off the proxy core, offlines its hyperthread sibling
```

It compares the three machines (sysctls, governor, MTU, offloads, kernel, Docker, git commit), measures both links with `iperf3`, and writes `bench/results/preflight-<ts>.txt`. It refuses (exit 1) on any difference or a link under `MIN_GBIT` (default 8). Keep the file: it documents the environment of the numbers.

### 5. Smoke test (about 15 minutes)

One proxy, one test, to prove the wiring before spending hours:

```sh
BENCH_ONLY=http-1kb REPEAT=1 REMOTE=1 ./bench/run-remote.sh rustle on
BENCH_ONLY=http-1kb REPEAT=1 REMOTE=1 ./bench/run-remote.sh 3proxy on
```

Check in `results.txt`: `err_kinds=` empty, `proxy_util` near 1.00 at the best level, no `proxy_unsaturated` (if it shows, the sweep needs a higher level for that proxy: edit `sweep_levels` in `matrix.sh`), `gen_saturated` absent. If the generator or origin is saturated, raise `GEN_THREADS` / `UP_THREADS`.

### 6. The campaigns

```sh
REMOTE=1 ./bench/run-all.sh                  # HTTP/CONNECT (plain, TLS, TLS full handshake, h2) + WebSocket
                                             # -> report, validity check, charts, README table
```

About 4 to 5 hours (the sweep multiplies the load tests). The script keeps going if a proxy fails and says so at the end.

Multi-core curve (the proxy host needs 2x the cores plus the harness's; with a 10 Gbit/s link, stay at 4 cores or below at 1 KB):

```sh
for n in 1 2 4; do
  PROXY_CPUS=0-$((n-1)) WORKERS=$n REMOTE=1 BENCH_ONLY=http-1kb ./bench/run-campaign.sh
done
```

(`WORKERS` always equals the number of cores in `PROXY_CPUS`; the script refuses otherwise.)

### 7. Review

`bench/results/report-<ts>.md` ends with the validity check. Every line it prints needs an answer before publishing: invalid point, proxy under 90 % of its cores, origin or sink saturated, a proxy with no result (it is then listed as unsupported, never as zero), different TLS ciphers. `dmesg | grep -i oom` on the proxy host after the tunnel tests.

### 8. Destroy the machines

Delete the servers in the console and check they disappear from the billing page.

## Phase B: tunnel throughput on the Mac

Quiet Mac, Docker running (the containers run in a Linux VM: OrbStack or Docker Desktop, give it all the CPUs and 8 GB or more).

```sh
cd bench
PROXIES="rustle 3proxy squid gost tinyproxy" MODES=plain BENCH_ONLY=flood-8 \
  GEN_THREADS=4 SINK_THREADS=4 UP_THREADS=2 ./run-campaign.sh
```

One CPU for the proxy, the rest for the generator and the sink (`env.sh` reads the VM's CPUs). Note the VM's name and CPU count in the commit message or RESULTS.md.

## Publish

```sh
FLOOD_FROM=bench/results/campaign-<mac-index>.txt REMOTE=1 REPORT_ONLY=1 ./bench/run-all.sh
git diff                     # README table + docs/benchmarks/*.svg
```

Then update `bench/RESULTS.md` (environment, preflight file, dates), AGENTS.md "Known state" (campaign files), and commit. `make bench-publish` is `run-all.sh`.

## If something looks wrong

* A proxy under 90 % CPU at its best level: add a higher level in `sweep_levels` (3proxy needed ~1000 connections at 1 KB).
* Errors at high concurrency: look at `err_kinds`; a `502` storm usually means the proxy's upstream pool or ephemeral ports (see `AGENTS.md`, invariant 5).
* Numbers that jump between passes: another workload on the machine, a thermal or frequency change (preflight records the governor), or TIME_WAIT left over: raise `SWEEP_PAUSE`.
