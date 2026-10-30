# Final benchmark: runbook

Two phases, because a NIC caps tunnel throughput and a single host over loopback does not:

| Phase | Where | What it measures | Result goes to |
|---|---|---|---|
| **A** | three machines (generator → proxy → origin), private network | requests/s, latency, TLS (resumed and full handshake), WebSocket, idle tunnels (RAM), CPU per request, and the multi-core curve | the README table and charts |
| **B** | one Linux host (Docker, loopback) | tunnel throughput (`flood`): no NIC in the way, the proxy and the kernel are the limit | the "Tunnels" row and chart (`FLOOD_FROM`) |

The README says which environment each number comes from.

## 0. Before anything

* `git status` clean and pushed, `make check` green. Every machine runs the same commit (preflight checks it).
* Read `FAIRNESS.md`: versions and settings of the competitors, and what is deliberately not tuned.
* Commit nothing and start no other workload on the machines during a run. For phase B, stop every other container and heavy process on the host (the numbers are only as quiet as the machine).

## Phase A: three machines

### 1. Machines

Scaleway Elastic Metal (hourly billing, never monthly), same zone, the same Private Network attached to all three. See `REMOTE.md` "Renting the machines". The link between two machines is as fast as the slower end, so use the I series (25 Gbit/s private) everywhere. Used: generator and proxy EM-I320E (24C/48T), origin + sink EM-I220E. Ubuntu 24.04 on each, your SSH key. Run the campaigns over the private addresses (the public interface is 1 Gbit/s).

### 2. Bootstrap (once per machine)

```sh
for h in $GEN_HOST $PROXY_HOST $UP_HOST; do
  ssh $h 'sudo bash -s' < bench/scripts/remote/bootstrap.sh
  ssh $h 'git clone https://github.com/EdouardCourty/rustle ~/rustle'   # or git pull
done
```

Private network (Elastic Metal: a VLAN on the NIC that carries the public address). Attach the three servers to one Private Network in the console, note each server's VLAN id (it can differ between servers) and private IP, then:

```sh
ssh $GEN_HOST 'sudo bash -s -- <vlan> <ip>/<prefix>' < bench/scripts/remote/private-net.sh   # same for the other two
ssh $GEN_HOST 'iperf3 -c <proxy private ip> -P 4 -t 5'                               # expect ~23 Gbit/s on 25 Gbit/s
```

### 3. Settings used for every command below

```sh
export GEN_HOST=...   PROXY_HOST=...   UP_HOST=...        # ssh targets
export GEN_ADDR=10.x  PROXY_ADDR=10.y  UP_ADDR=10.z        # private addresses
export PROXY_CPUS=0 WORKERS=1                              # one core for every proxy
export MIN_GBIT=20                                         # 25 Gbit/s private network
```

### 4. Preflight (always, before a campaign)

```sh
APPLY=1 bench/scripts/preflight.sh          # moves NIC interrupts off the proxy core, offlines its hyperthread sibling
```

It compares the three machines (sysctls, governor, MTU, offloads, kernel, Docker, git commit), measures both links with `iperf3`, and writes `bench/results/preflight-<ts>.txt`. It refuses (exit 1) on any difference or a link under `MIN_GBIT` (default 8; use 20 on a 25 Gbit/s setup). Keep the file: it documents the environment of the numbers.

### 5. Smoke test (about 15 minutes)

One proxy, one test, to prove the wiring before spending hours:

```sh
BENCH_ONLY=http-1kb REPEAT=1 REMOTE=1 bench/scripts/run-remote.sh rustle on
BENCH_ONLY=http-1kb REPEAT=1 REMOTE=1 bench/scripts/run-remote.sh 3proxy on
```

Check in `results/<label>/remote-auth-on/<timestamp>/run.json` (or let `python3 bench/tools/aggregate.py --report --sources <manifest>` list the points): `err_kinds` empty, `proxy_util` near 1.00 at the best level, no `proxy_unsaturated` flag (if it shows, the sweep needs a higher level for that proxy: edit `sweep_levels` in `matrix.sh`), no `generator_saturated` flag. If the generator or origin is saturated, raise `GEN_THREADS` / `UP_THREADS`.

### 6. The campaigns

```sh
REMOTE=1 bench/scripts/run-all.sh                  # HTTP/CONNECT (plain, TLS, TLS full handshake, h2) + WebSocket
                                             # -> report, validity check, charts, README table
```

About 4 to 5 hours (the sweep multiplies the load tests). The script keeps going if a proxy fails and says so at the end.

Multi-core curve (generator and origin hosts need at least as many cores as the proxy, preferably twice; re-run `APPLY=1 bench/scripts/preflight.sh` with the new `PROXY_CPUS` before each size, since the offlined siblings change):

```sh
for n in 1 2 4 8; do
  PROXY_CPUS=0-$((n-1)) WORKERS=$n REMOTE=1 BENCH_ONLY=http-1kb bench/scripts/run-campaign.sh
done
```

(`WORKERS` always equals the number of cores in `PROXY_CPUS`; the script refuses otherwise.)

### 7. Review

`bench/results/report-<ts>.md` ends with the validity check. Every line it prints needs an answer before publishing: invalid point, proxy under 90 % of its cores, origin or sink saturated, a proxy with no result (it is then listed as unsupported, never as zero), different TLS ciphers. `dmesg | grep -i oom` on the proxy host after the tunnel tests.

### 8. Destroy the machines

Delete the servers in the console and check they disappear from the billing page.

## Phase B: tunnel throughput on a single host

One quiet Linux host with Docker (or a Docker VM given all its CPUs and 8 GB or more), everything over loopback: no NIC in the way. Required only when the three-machine link is the limit (flood above the link speed).

```sh
cd bench
PROXIES="rustle 3proxy squid gost tinyproxy" MODES=plain BENCH_ONLY=flood-8 \
  GEN_THREADS=4 SINK_THREADS=4 UP_THREADS=2 ./scripts/run-campaign.sh
```

One CPU for the proxy, the rest for the generator and the sink (`env.sh` reads the VM's CPUs). Note the host's model and CPU count in RESULTS.md.

## Publish

```sh
FLOOD_FROM=bench/results/campaign-<single-host-manifest>.json REMOTE=1 REPORT_ONLY=1 bench/scripts/run-all.sh
git diff                     # benchmarks.json, README table and numbers, RESULTS.md, bench/charts/*.svg
```

Every table and number of `bench/documentation/RESULTS.md`, the README table and the charts are written from the raw runs. By hand: the new campaign's folder under `bench/data/` (`campaign.json`, `proxies.json`, `index.json`, see `bench/data/README.md`), the prose of `RESULTS.md`, AGENTS.md "Known state" (campaign files), and copy the runs you publish into `bench/published/<date>/` (point the campaign's `sources` at them). `make data-check` must pass. `make bench-publish` is `run-all.sh`.

## If something looks wrong

* A proxy under 90 % CPU at its best level: add a higher level in `sweep_levels` (3proxy needed ~1000 connections at 1 KB).
* Errors at high concurrency: look at `err_kinds`; a `502` storm usually means the proxy's upstream pool or ephemeral ports (see `AGENTS.md`, invariant 5).
* Numbers that jump between passes: another workload on the machine, a thermal or frequency change (preflight records the governor), or TIME_WAIT left over: raise `SWEEP_PAUSE`.
