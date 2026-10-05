#!/usr/bin/env bash
# Benchmark one proxy (rustle | 3proxy) inside Docker.
#
# Usage:
#   ./run-bench.sh [rustle|3proxy] [on|off]
#   REPEAT=5 ./run-bench.sh rustle on     # more repetitions
#   ALLOCATOR="--features mimalloc" ./run-bench.sh rustle on   # allocator A/B
#
# Each client invocation does its own warmup + REPEAT measured runs and
# prints one "RESULT median ..." line. This script additionally samples the
# proxy container's CPU (utime+stime of PID 1) and RSS around each test and
# appends: cpu_s_per_run=... rss_mb=...
#
# Layout (2 cores): proxy pinned to core 0; generator/upstream/sink on core 1.
# Results: bench/results/<proxy>/auth-<on|off>/<timestamp>/results.txt
set -euo pipefail
cd "$(dirname "$0")"

PROXY="${1:-rustle}"
AUTH="${2:-on}"
REPEAT="${REPEAT:-3}"
# Multi-core: WORKERS = shards/workers of the proxy (DUTs that support it),
# PROXY_NCPU = CPUs given to the proxy (env.sh), GEN_THREADS = generator
# runtime threads (size to BENCH_CPU or the generator saturates first).
export WORKERS="${WORKERS:-1}"
export ACCESS_LOG="${ACCESS_LOG:-off}"   # rustle: off | errors | all
export KTLS="${KTLS:-false}"               # rustle TLS listener: kernel TLS on/off
export SOCK_BUF="${SOCK_BUF:-1048576}"      # rustle socket buffers (0 = kernel autotuning, like the other DUTs)
# SECCOMP=unconfined: proxy container without Docker's seccomp profile
# (needed by io_uring DUTs; use it for every DUT of such a comparison).
if [[ "${SECCOMP:-default}" == unconfined ]]; then
    export COMPOSE_FILE=docker-compose.yml:docker-compose.seccomp.yml
fi
GEN_THREADS="${GEN_THREADS:-2}"
# Origin and sink threads (compose reads them): size to the CPUs they share.
export UP_THREADS="${UP_THREADS:-1}" SINK_THREADS="${SINK_THREADS:-1}"
# PROXY_TLS=1: HTTPS proxy benchmark (client -> proxy over TLS).
# PROXY_H2=1: HTTP/2 over TLS (ALPN h2). Ports come from the DUT's
# "# bench-ports: tls=<p> h2=<p>" line; a DUT without one lacks the mode.
PROXY_TLS="${PROXY_TLS:-0}"
PROXY_H2="${PROXY_H2:-0}"
GEN_ARGS=(--threads "$GEN_THREADS")
LABEL="$PROXY"
_port() { sed -n "s/^# bench-ports:.*$1=\([0-9]*\).*/\1/p" "dut/$PROXY/Dockerfile"; }
if [[ "$PROXY_H2" == 1 ]]; then
    p=$(_port h2)
    [[ -n "$p" ]] || { echo "$PROXY has no HTTP/2 proxy listener (PROXY_H2=1)" >&2; exit 2; }
    GEN_ARGS+=(--proxy "proxy:$p" --proxy-h2)
    LABEL="$PROXY-h2"
elif [[ "$PROXY_TLS" == 1 ]]; then
    p=$(_port tls)
    [[ -n "$p" ]] || { echo "$PROXY has no HTTPS proxy listener (PROXY_TLS=1)" >&2; exit 2; }
    GEN_ARGS+=(--proxy "proxy:$p" --proxy-tls)
    LABEL="$PROXY-tls"
fi
# MATRIX=ws: WebSocket workloads (matrix.sh); results land under <label>-ws.
if [[ "${MATRIX:-}" == ws ]]; then
    [[ "$PROXY_H2" == 1 ]] && { echo "MATRIX=ws: WebSocket upgrades are HTTP/1.1 only (no PROXY_H2)" >&2; exit 2; }
    LABEL="$LABEL-ws"
fi
HOLD_MAX_SINGLE=30000   # above this, split across 2 client replicas (ephemeral ports)

export PROXY_IMAGE="${PROXY}:bench"
export AUTH

# Auto-detected CPU pinning (shared with the Makefile).
. "$(dirname "$0")/env.sh"
echo "==> cpu pinning: proxy=$PROXY_CPU generator/upstream/sink=$BENCH_CPU (available: $ALL_CPUS)"

echo "==> building images (serialized: rustc peaks ~1-2 GB transiently)"
docker compose build generator >/dev/null
docker compose build upstream >/dev/null
docker build -q -t "$PROXY_IMAGE" ${ALLOCATOR:+--build-arg ALLOCATOR="$ALLOCATOR"} -f "dut/$PROXY/Dockerfile" .. >/dev/null

echo "==> starting stack (proxy=$PROXY auth=$AUTH repeat=$REPEAT)"
docker compose up -d upstream sink proxy >/dev/null
trap 'docker compose down >/dev/null 2>&1' EXIT
sleep 1

out="results/${LABEL}/auth-${AUTH}/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$out"
: > "$out/results.txt"
echo "# $PROXY auth=$AUTH tls=$PROXY_TLS h2=$PROXY_H2 repeat=$REPEAT workers=$WORKERS access_log=$ACCESS_LOG ktls=$KTLS seccomp=${SECCOMP:-default} proxy_cpu=$PROXY_CPU bench_cpu=$BENCH_CPU gen_threads=$GEN_THREADS up_threads=$UP_THREADS sink_threads=$SINK_THREADS $(date)" | tee -a "$out/results.txt"

# CPU ticks and RSS (MB) of the proxy under test.
# Whole container (cgroup v2), not PID 1: multi-process DUTs (Squid SMP
# workers) would otherwise hide their workers' CPU. Unit: 1/100 s.
cpu_ticks() { docker compose exec -T proxy cat /sys/fs/cgroup/cpu.stat | awk '/^usage_usec/{printf "%d", $2/10000}' || echo 0; }
# Sum over the container's processes (same reason), minus the sh/cat of
# this probe itself.
_sum_status() { docker compose exec -T proxy sh -c "cat /proc/[0-9]*/status 2>/dev/null" | awk -v k="$1:" '$1=="Name:"{n=$2} $1==k && n!="sh" && n!="cat"{s+=$2} END{printf "%.1f", s/1024}' || echo 0; }
rss_mb() { _sum_status VmRSS; }
# Peak RSS of the test (VmHWM; the proxy restarts before each test). rss_mb
# is sampled after the load ended = memory still held, not the live cost.
peak_rss_mb() { _sum_status VmHWM; }
# TCP counters of the proxy's network namespace (/proc/net/snmp, netstat):
# segments sent, connections opened upstream (pool churn), retransmits,
# delayed ACKs, accept-queue overflows/drops.
tcp_counters() {
    docker compose exec -T proxy cat /proc/net/snmp /proc/net/netstat 2>/dev/null | awk '
        /^(Tcp|TcpExt):/ { if (!(($1) in hdr)) { for (i=2;i<=NF;i++) k[$1,i]=$i; hdr[$1]=1 }
                           else { for (i=2;i<=NF;i++) v[k[$1,i]]=$i } }
        END { printf "%d %d %d %d %d %d", v["OutSegs"], v["ActiveOpens"], v["RetransSegs"],
              v["DelayedACKs"], v["ListenOverflows"], v["ListenDrops"] }' || echo "0 0 0 0 0 0"
}
# Per measured run (segments, opens, retrans, delayed ACKs); accept-queue
# overflows/drops as totals (any > 0 explains SYN retransmits, p99 ~1/3/5 s).
tcp_delta() { # <before> <after> <repeat>
    awk -v a="$1" -v b="$2" -v n="$3" 'BEGIN { split(a,x," "); split(b,y," ");
        printf "segs_out_per_run=%d active_opens_per_run=%d retrans_per_run=%d delayed_acks_per_run=%d listen_overflows=%d listen_drops=%d",
            (y[1]-x[1])/n, (y[2]-x[2])/n, (y[3]-x[3])/n, (y[4]-x[4])/n, y[5]-x[5], y[6]-x[6] }'
}
# Same, summed over all upstream processes (nginx master+workers): the
# harness-side cost per test is the ceiling nobody sees.
up_ticks() {
    docker compose exec -T upstream sh -c \
        'cat /proc/[0-9]*/stat 2>/dev/null | awk "{s+=\$14+\$15} END{print s+0}"'
}

run_test() {
    local name="$1"; shift
    echo
    echo "### $name"
    docker compose restart proxy >/dev/null  # fresh state + fresh CPU counters
    sleep 0.5
    local tick0 tick1 median cpu_s_per_run up0 up1 up_cpu_s_per_run tcp0 tcp1
    # Warmup in its own generator run, before the counters are sampled:
    # the proxy's CPU then covers the measured runs only (it used to
    # include the warmup, amortized over REPEAT: ~+6 % on every figure).
    docker compose --profile generator run --rm -T generator \
        --warmup-only "${GEN_ARGS[@]}" "$@" >/dev/null 2>&1 || true
    tick0=$(cpu_ticks)
    up0=$(up_ticks)
    tcp0=$(tcp_counters)
    docker compose --profile generator run --rm -T generator \
        --repeat "$REPEAT" --no-warmup "${GEN_ARGS[@]}" "$@" | tee /dev/stderr | grep '^RESULT median' > "$out/.median.txt" \
        || true  # no RESULT (generator/proxy died): reported below, the matrix goes on
    tick1=$(cpu_ticks)
    up1=$(up_ticks)
    tcp1=$(tcp_counters)
    median=$(cat "$out/.median.txt")
    if [ -z "$median" ]; then
        echo "!! no RESULT median line for $name" | tee -a "$out/results.txt"
        return
    fi
    # CPU seconds attributable to one measured run (warmup amortized over REPEAT).
    cpu_s_per_run=$(awk -v a="$tick1" -v b="$tick0" -v n="$REPEAT" \
        'BEGIN{printf "%.3f", (a-b)/100/n}')
    up_cpu_s_per_run=$(awk -v a="$up1" -v b="$up0" -v n="$REPEAT" \
        'BEGIN{printf "%.2f", (a-b)/100/n}')
    echo "$median cpu_s_per_run=$cpu_s_per_run up_cpu_s_per_run=$up_cpu_s_per_run rss_mb=$(rss_mb) peak_rss_mb=$(peak_rss_mb) $(tcp_delta "$tcp0" "$tcp1" "$REPEAT")" | tee -a "$out/results.txt"
}

# Hold with optional 2-replica split (single client exhausts ephemeral ports).
run_hold() {
    local conns="$1" duration="${2:-10}"
    if (( conns <= HOLD_MAX_SINGLE )); then
        run_test "hold-${conns}" hold sink:9000 --conns "$conns" --duration "$duration" --ramp 4096
        return
    fi
    # split across 2 replicas, single run each (aggregate)
    echo
    echo "### hold-${conns} (2 replicas)"
    docker compose restart proxy >/dev/null
    sleep 0.5
    local half=$(( (conns+1)/2 )) tick0 tick1
    tick0=$(cpu_ticks)
    for i in 1 2; do
        docker compose --profile generator run -d --name "holdrep-$i" \
            generator --repeat 1 --no-warmup "${GEN_ARGS[@]}" hold sink:9000 \
            --conns "$half" --duration "$duration" --ramp 4096 >/dev/null
    done
    for i in 1 2; do docker wait "holdrep-$i" >/dev/null; done
    tick1=$(cpu_ticks)
    local opened=0 failed=0 maxelapsed=0
    for i in 1 2; do
        local line o f e
        line=$(docker logs "holdrep-$i" 2>/dev/null | grep '^RESULT' | tail -1) || true
        docker rm -f "holdrep-$i" >/dev/null 2>&1 || true
        [ -n "$line" ] || continue
        o=$(sed -n 's/.* opened=\([0-9.]*\).*/\1/p' <<<"$line")
        f=$(sed -n 's/.* failed=\([0-9.]*\).*/\1/p' <<<"$line")
        e=$(sed -n 's/.* elapsed_s=\([0-9.]*\).*/\1/p' <<<"$line")
        opened=$(( opened + ${o%.*} ))
        failed=$(( failed + ${f%.*} ))
        maxelapsed=$(awk -v a="$maxelapsed" -v b="$e" 'BEGIN{print (a>b)?a:b}')
    done
    local rate cpu_s_per_run
    rate=$(awk -v o="$opened" -v e="$maxelapsed" 'BEGIN{printf "%.0f", o/e}')
    cpu_s_per_run=$(awk -v a="$tick1" -v b="$tick0" 'BEGIN{printf "%.3f", (a-b)/100}')
    echo "RESULT median mode=hold conns=$conns replicas=2 opened=$opened failed=$failed rate=$rate elapsed_s=$maxelapsed cpu_s_per_run=$cpu_s_per_run rss_mb=$(rss_mb) peak_rss_mb=$(peak_rss_mb)" | tee -a "$out/results.txt"
}

# --- Network warmup (before any measurement): conntrack growth, DNS/kernel
# caches, autotuned TCP windows — without it the first tests of the matrix
# absorb the warmup and the variance between passes is ±30 %.
echo "### warmup-network"
docker compose --profile generator run --rm -T generator --repeat 1 --no-warmup "${GEN_ARGS[@]}" \
    http http://upstream:8000/1mb --conns 16 --requests 200 >/dev/null 2>&1 || true
docker compose --profile generator run --rm -T generator --repeat 1 --no-warmup "${GEN_ARGS[@]}" \
    flood sink:9000 --conns 8 --duration 3 >/dev/null 2>&1 || true

# --- Workloads: list, seeded shuffle and BENCH_ONLY live in matrix.sh.
. ./matrix.sh
matrix_select_and_shuffle

for entry in "${shuffled[@]}"; do
    eval "$entry"
done

echo
echo "==> results in $out/results.txt"
