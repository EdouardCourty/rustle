# Three-machine benchmark

The full procedure (machines, preflight, campaigns, publishing) is in [`RUNBOOK.md`](RUNBOOK.md); this page is the reference for the pieces.

On one host the load generator, origin and sink share cores with (or next to) the proxy and cap the load: a fast proxy measures the harness. Here every role has its own machine.

```
GEN_HOST  (load generator)  ──►  PROXY_HOST  (proxy under test)  ──►  UP_HOST  (origin + sink)
```

Same matrix, seed, shuffle and `RESULT` lines as `run-bench.sh` (shared `matrix.sh`), driven by `run-remote.sh` over SSH. Containers use the host network.

## Machines

*   Three machines in one private network, ideally identical. **Bandwidth is part of the test**: the flood tests push tens of Gbit/s, so use a private link of at least 10 Gbit/s (otherwise flood measures the NIC).
*   Proxy host: as many cores as the largest point of the scaling curve (`NCPUS`). Generator and origin hosts: at least as many cores as the proxy host, preferably twice (the generator needs ~7 µs of CPU per request).
*   Docker and SSH key access (`BatchMode`) from the machine you run the script on, to all three. The repo is checked out at the same path on every host (`REMOTE_DIR`, default `~/rustle`); `git pull` it everywhere before a run.

## Renting the machines

Three on-demand machines in the same region and private network, **dedicated cores** (shared or burstable vCPUs make a proxy benchmark meaningless), billed by the hour: a full campaign takes a working day, so a few tens of euros.

*   **Scaleway Elastic Metal, I series (recommended).** Bare metal: dedicated cores, no noisy neighbour, hyperthread siblings and NIC interrupts really controllable (`remote/apply-proxy-host.sh`), hourly billing without commitment, 25 Gbit/s private network. The link between two machines is as fast as the slower end, so use the same series on all three: e.g. EM-I320E (24C/48T) for the proxy and the generator, EM-I220E or EM-I120E for the origin. Attach the same Private Network to every server and use the private addresses (the public interface is 1 Gbit/s).
*   **Other providers**: any bare-metal or dedicated-vCPU offer with a private network of at least 10 Gbit/s in one region (Hetzner Cloud CCX, DigitalOcean Premium CPU-Optimized). Check the per-instance network speed, not the monthly transfer allowance: it is a billing quota, not a speed.
*   **Avoid** shared-vCPU or burstable classes, instances whose bandwidth scales with the vCPU count (Scaleway Instances: 200 Mbit/s per vCPU), and AWS "up to 12.5 Gbit/s" instances (baseline 3 Gbit/s, burst credits: flood results would measure the credits).

Before the first run, measure what you actually have: `iperf3 -P 4 -c <origin>` from the generator and the proxy host. The tunnel (`flood`) numbers are capped by that link; the request-rate, latency and CPU-per-request numbers are not. If the link is under ~10 Gbit/s, read the flood results as CPU per GB, not as Gbit/s.

Setup per machine: Docker, this repository at the same path, SSH key access from your workstation, the sysctls below. Destroy the machines when the campaign is done.

## Host settings (all three, once)

```sh
sudo sysctl -w net.core.somaxconn=65535 net.ipv4.ip_local_port_range="1024 65535" net.ipv4.tcp_tw_reuse=1
sudo sysctl -w net.core.rmem_max=16777216 net.core.wmem_max=16777216
# Thread-per-connection proxies (3proxy, Tinyproxy) hit these walls long before 25,000 tunnels:
sudo sysctl -w vm.max_map_count=1048576 kernel.pid_max=4194304 kernel.threads-max=4194304
```

The proxy is the active closer of the tunnels to the sink: without `tcp_tw_reuse` and a wide port range, TIME_WAIT sockets exhaust the ephemeral ports. Open the proxy port (8080, plus the TLS/h2 ports), 8000 and 9000 between the machines. Keep `net.core.somaxconn` and the other sysctls identical for every proxy compared.

## Run

```sh
export GEN_HOST=bench-a PROXY_HOST=bench-b UP_HOST=bench-c
export GEN_ADDR=10.0.0.1 PROXY_ADDR=10.0.0.2 UP_ADDR=10.0.0.3   # private addresses

./bench/run-remote.sh rustle on                                  # whole matrix, 1 shard, all CPUs
WORKERS=4 PROXY_CPUS=0-3 ./bench/run-remote.sh rustle on         # 4 cores
BENCH_ONLY=http-1kb ./bench/run-remote.sh squid on               # one test
PROXY_TLS=1 ./bench/run-remote.sh rustle on                      # HTTPS proxy (PROXY_H2=1: HTTP/2)

REMOTE=1 NCPUS="1 2 4 8" PROXIES="rustle 3proxy squid gost" ./bench/run-scaling.sh   # scaling curve
DRY_RUN=1 ./bench/run-remote.sh rustle on                        # print the commands only

REMOTE=1 ./bench/run-campaign.sh                                 # the whole campaign (5 proxies x plain/TLS/h2) on the three machines
REMOTE=1 MATRIX=ws ./bench/run-campaign.sh                       # the WebSocket campaign
```

Results land in `bench/results/<proxy>/remote-auth-<on|off>/<timestamp>/results.txt` on the machine you run from; the header records the hosts and parameters.

## Valid results

*   **The generator must not be saturated.** `run-remote.sh` appends `gen_saturated=1` to a `RESULT` line when the generator used more than 70 % of its threads: raise `GEN_THREADS` or use a bigger generator host, and discard the point. Same check by hand: the proxy's CPU should be at its cpuset limit while the generator and the origin are not.
*   `err_kinds` must be empty, `retrans_per_run` / `listen_drops` close to 0.
*   Compare every proxy with the same host settings, same `PROXY_CPUS` and same seed. Noise is about ±3–5 %: below that, no conclusion. Do not compare with numbers from the single-host harness (`run-bench.sh`).
*   Do not edit the repo or the scripts during a run, and build images one machine at a time if they are small (rustc peaks at 1–2 GB).
