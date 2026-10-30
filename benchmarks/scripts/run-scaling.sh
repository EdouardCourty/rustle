#!/usr/bin/env bash
# Multi-core scaling curve: for each proxy and each core count n, give the
# proxy n CPUs (and n workers when it takes a count), the harness the rest,
# and run the matrix (or one step with BENCH_ONLY) at the fixed seed.
#
# Usage (>= 2*max(n) CPUs recommended):
#   benchmarks/scripts/run-scaling.sh                                  # defaults below
#   PROXIES="rustle squid" NCPUS="1 2 4 8" AUTH=on benchmarks/scripts/run-scaling.sh
#   BENCH_ONLY=http-1kb benchmarks/scripts/run-scaling.sh              # one step per point
#   PROXY_TLS=1 benchmarks/scripts/run-scaling.sh                      # HTTPS proxy
#   PROXY_H2=1 benchmarks/scripts/run-scaling.sh                       # HTTP/2 proxy (TLS, ALPN h2)
#   REMOTE=1 GEN_HOST=a PROXY_HOST=b UP_HOST=c NCPUS="1 2 4 8" benchmarks/scripts/run-scaling.sh
#                                                     # three machines (run-remote.sh, REMOTE.md)
#
# Each point is a regular run-bench.sh run (results/<label>/auth-<a>/<ts>/run.json,
# which records workers/proxy_cpu/bench_cpu/gen_threads); the manifest of the points
# (role `scaling`, one entry per proxy and core count) is written to
# results/scaling-<ts>.json, to paste into the `sources` of campaign.json.
set -euo pipefail
SCRIPTS="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPTS/.."   # the benchmark root: proxies/, results/, tools/ and the Docker contexts are relative to it

PROXIES="${PROXIES:-rustle 3proxy squid gost tinyproxy}"
# The transport of the points (the same for all of them), as the manifest names it.
scaling_transport() { if [[ "${PROXY_H2:-0}" == 1 ]]; then echo h2; elif [[ "${PROXY_TLS:-0}" == 1 ]]; then echo tls; else echo plain; fi; }
NCPUS="${NCPUS:-1 2 4}"
AUTH="${AUTH:-on}"
export BENCH_SEED="${BENCH_SEED:-2004113323}"

if [[ "${REMOTE:-0}" == 1 ]]; then
    # Three machines: the proxy host's CPUs are its own, the harness is elsewhere.
    : "${PROXY_HOST:?set PROXY_HOST}" "${GEN_HOST:?set GEN_HOST}" "${UP_HOST:?set UP_HOST}"
    total=$(ssh -o BatchMode=yes "$PROXY_HOST" nproc)
    manifest="results/scaling-remote-$(date +%Y%m%d-%H%M%S).json"
    mkdir -p results
    echo "# scaling remote proxies=[$PROXIES] ncpus=[$NCPUS] auth=$AUTH tls=${PROXY_TLS:-0} h2=${PROXY_H2:-0} seed=$BENCH_SEED proxy_cpus=$total"
    for n in $NCPUS; do
        if (( n > total )); then echo "skip n=$n: proxy host has $total CPUs"; continue; fi
        for p in $PROXIES; do
            echo "==> $p n=$n"
            PROXY_CPUS="0-$((n - 1))" WORKERS="$n" "$SCRIPTS/run-remote.sh" "$p" "$AUTH" | tee /dev/stderr | while IFS= read -r line; do
                case "$line" in
                    "==> run in "*) python3 tools/record.py manifest "$manifest" --role scaling --cores "$n" --proxy "$p" --transport "$(scaling_transport)" --run "${line#==> run in }" ;;
                esac
            done
        done
    done
    echo "==> manifest: $manifest"
    exit 0
fi

total=$(awk -F',' '{ n=0; for (i=1;i<=NF;i++) { split($i,a,"-");
    n += (a[2]==""?1:a[2]-a[1]+1) } print n }' \
    <<<"$(cat /sys/fs/cgroup/cpuset.cpus.effective 2>/dev/null \
        || grep Cpus_allowed_list /proc/self/status 2>/dev/null | awk '{print $2}' \
        || true)")
# macOS: the CPUs of Docker's Linux VM
[[ "$total" -ge 1 ]] 2>/dev/null && [[ -n "$(cat /sys/fs/cgroup/cpuset.cpus.effective 2>/dev/null)" ]] \
    || total=$(docker info --format '{{.NCPU}}')
manifest="results/scaling-$(date +%Y%m%d-%H%M%S).json"
mkdir -p results
echo "# scaling proxies=[$PROXIES] ncpus=[$NCPUS] auth=$AUTH tls=${PROXY_TLS:-0} h2=${PROXY_H2:-0} seed=$BENCH_SEED cpus=$total"

for n in $NCPUS; do
    if (( n >= total )); then
        echo "skip n=$n: only $total CPUs (the harness needs at least one)"
        continue
    fi
    for p in $PROXIES; do
        if [[ "${PROXY_H2:-0}" == 1 ]] && ! grep -q '^# bench-ports:.*h2=' "proxies/$p/Dockerfile"; then
            echo "skip $p: no HTTP/2 proxy listener"
            continue
        elif [[ "${PROXY_TLS:-0}" == 1 ]] && ! grep -q '^# bench-ports:.*tls=' "proxies/$p/Dockerfile"; then
            echo "skip $p: no HTTPS proxy listener"
            continue
        fi
        echo "==> $p n=$n"
        # Fresh env per point (env.sh caches its split in the process).
        env -u _BENCH_ENV_LOADED -u PROXY_CPU -u BENCH_CPU \
            PROXY_NCPU="$n" WORKERS="$n" GEN_THREADS="$(( total - n ))" \
            "$SCRIPTS/run-bench.sh" "$p" "$AUTH" | tee /dev/stderr | while IFS= read -r line; do
                case "$line" in
                    "==> run in "*) python3 tools/record.py manifest "$manifest" --role scaling --cores "$n" --proxy "$p" --transport "$(scaling_transport)" --run "${line#==> run in }" ;;
                esac
            done
    done
done
echo "==> manifest: $manifest"
