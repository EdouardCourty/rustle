# Three-machine benchmark

On one host the load generator, origin and sink share cores with (or next to) the proxy and cap the load: a fast proxy measures the harness. Here every role has its own machine.

```
GEN_HOST  (load generator)  ──►  PROXY_HOST  (proxy under test)  ──►  UP_HOST  (origin + sink)
```

Same matrix, seed, shuffle and `RESULT` lines as `run-bench.sh` (shared `matrix.sh`), driven by `run-remote.sh` over SSH. Containers use the host network.

## Machines

*   Three machines in one private network, ideally identical. **Bandwidth is part of the test**: the flood tests push tens of Gbit/s, so use a private link of at least 10 Gbit/s (otherwise flood measures the NIC).
*   Proxy host: as many cores as the largest point of the scaling curve (`NCPUS`). Generator and origin hosts: at least as many cores as the proxy host, preferably twice (the generator needs ~7 µs of CPU per request).
*   Docker and SSH key access (`BatchMode`) from the machine you run the script on, to all three. The repo is checked out at the same path on every host (`REMOTE_DIR`, default `~/rustle`); `git pull` it everywhere before a run.

## Host settings (all three, once)

```sh
sudo sysctl -w net.core.somaxconn=65535 net.ipv4.ip_local_port_range="1024 65535" net.ipv4.tcp_tw_reuse=1
sudo sysctl -w net.core.rmem_max=16777216 net.core.wmem_max=16777216
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
```

Results land in `bench/results/<proxy>/remote-auth-<on|off>/<timestamp>/results.txt` on the machine you run from; the header records the hosts and parameters.

## Valid results

*   **The generator must not be saturated.** `run-remote.sh` appends `gen_saturated=1` to a `RESULT` line when the generator used more than 70 % of its threads: raise `GEN_THREADS` or use a bigger generator host, and discard the point. Same check by hand: the proxy's CPU should be at its cpuset limit while the generator and the origin are not.
*   `err_kinds` must be empty, `retrans_per_run` / `listen_drops` close to 0.
*   Compare every proxy with the same host settings, same `PROXY_CPUS` and same seed. Noise is about ±3–5 %: below that, no conclusion. Do not compare with numbers from the single-host harness (`run-bench.sh`).
*   Do not edit the repo or the scripts during a run, and build images one machine at a time if they are small (rustc peaks at 1–2 GB).
