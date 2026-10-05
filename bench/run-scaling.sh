#!/usr/bin/env bash
# Multi-core scaling curve: for each proxy and each core count n, give the
# proxy n CPUs (and n workers when it takes a count), the harness the rest,
# and run the matrix (or one step with BENCH_ONLY) at the fixed seed.
#
# Usage (VPS, >= 2*max(n) CPUs recommended):
#   ./run-scaling.sh                                  # defaults below
#   PROXIES="rustle squid" NCPUS="1 2 4 8" AUTH=on ./run-scaling.sh
#   BENCH_ONLY=http-1kb ./run-scaling.sh              # one step per point
#   PROXY_TLS=1 ./run-scaling.sh                      # HTTPS proxy
#   PROXY_H2=1 ./run-scaling.sh                       # HTTP/2 proxy (TLS, ALPN h2)
#   REMOTE=1 GEN_HOST=a PROXY_HOST=b UP_HOST=c NCPUS="1 2 4 8" ./run-scaling.sh
#                                                     # three machines (run-remote.sh, REMOTE.md)
#
# Each point is a regular run-bench.sh run (results/<proxy>/auth-<a>/<ts>/,
# header line records workers/proxy_cpu/bench_cpu/gen_threads); the index of
# the points is written to results/scaling-<ts>.txt.
set -euo pipefail
cd "$(dirname "$0")"

PROXIES="${PROXIES:-rustle 3proxy squid gost tinyproxy}"
NCPUS="${NCPUS:-1 2 4}"
AUTH="${AUTH:-on}"
export BENCH_SEED="${BENCH_SEED:-2004113323}"

if [[ "${REMOTE:-0}" == 1 ]]; then
    # Three machines: the proxy host's CPUs are its own, the harness is elsewhere.
    : "${PROXY_HOST:?set PROXY_HOST}" "${GEN_HOST:?set GEN_HOST}" "${UP_HOST:?set UP_HOST}"
    total=$(ssh -o BatchMode=yes "$PROXY_HOST" nproc)
    index="results/scaling-remote-$(date +%Y%m%d-%H%M%S).txt"
    mkdir -p results
    echo "# scaling remote proxies=[$PROXIES] ncpus=[$NCPUS] auth=$AUTH tls=${PROXY_TLS:-0} h2=${PROXY_H2:-0} seed=$BENCH_SEED proxy_cpus=$total" | tee "$index"
    for n in $NCPUS; do
        if (( n > total )); then echo "skip n=$n: proxy host has $total CPUs" | tee -a "$index"; continue; fi
        for p in $PROXIES; do
            echo "==> $p n=$n"
            PROXY_CPUS="0-$((n - 1))" WORKERS="$n" ./run-remote.sh "$p" "$AUTH" | tee /dev/stderr | grep '^==> results in' \
                | sed "s/^==> results in /$p n=$n /" >> "$index"
        done
    done
    echo "==> index: $index"
    exit 0
fi

total=$(awk -F',' '{ n=0; for (i=1;i<=NF;i++) { split($i,a,"-");
    n += (a[2]==""?1:a[2]-a[1]+1) } print n }' \
    <<<"$(cat /sys/fs/cgroup/cpuset.cpus.effective 2>/dev/null \
        || grep Cpus_allowed_list /proc/self/status | awk '{print $2}')")
index="results/scaling-$(date +%Y%m%d-%H%M%S).txt"
mkdir -p results
echo "# scaling proxies=[$PROXIES] ncpus=[$NCPUS] auth=$AUTH tls=${PROXY_TLS:-0} h2=${PROXY_H2:-0} seed=$BENCH_SEED cpus=$total" | tee "$index"

for n in $NCPUS; do
    if (( n >= total )); then
        echo "skip n=$n: only $total CPUs (the harness needs at least one)" | tee -a "$index"
        continue
    fi
    for p in $PROXIES; do
        if [[ "${PROXY_H2:-0}" == 1 ]] && ! grep -q '^# bench-ports:.*h2=' "dut/$p/Dockerfile"; then
            echo "skip $p: no HTTP/2 proxy listener" | tee -a "$index"
            continue
        elif [[ "${PROXY_TLS:-0}" == 1 ]] && ! grep -q '^# bench-ports:.*tls=' "dut/$p/Dockerfile"; then
            echo "skip $p: no HTTPS proxy listener" | tee -a "$index"
            continue
        fi
        echo "==> $p n=$n"
        # Fresh env per point (env.sh caches its split in the process).
        env -u _BENCH_ENV_LOADED -u PROXY_CPU -u BENCH_CPU \
            PROXY_NCPU="$n" WORKERS="$n" GEN_THREADS="$(( total - n ))" \
            ./run-bench.sh "$p" "$AUTH" | tee /dev/stderr | grep '^==> results in' \
            | sed "s/^==> results in /$p n=$n /" >> "$index"
    done
done
echo "==> index: $index"
