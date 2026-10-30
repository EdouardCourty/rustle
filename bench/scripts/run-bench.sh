#!/usr/bin/env bash
# Benchmark one proxy (rustle | 3proxy) inside Docker.
#
# Usage:
#   bench/scripts/run-bench.sh [rustle|3proxy] [on|off]
#   REPEAT=5 bench/scripts/run-bench.sh rustle on     # more repetitions
#   ALLOCATOR="--features mimalloc" bench/scripts/run-bench.sh rustle on   # allocator A/B
#
# Each client invocation does its own warmup + REPEAT measured runs and prints
# one JSON document on stdout (its medians; a short human line per run goes to
# stderr). This script additionally takes raw snapshots of the proxy container's
# CPU, memory and network counters (and of the origin's and the sink's CPU) around
# each test; tools/record.py turns the two into the test's result in run.json.
#
# Layout (2 cores): proxy pinned to core 0; generator/upstream/sink on core 1.
# Results: bench/results/<label>/auth-<on|off>/<timestamp>/run.json
set -euo pipefail
SCRIPTS="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPTS/.."   # the benchmark root: dut/, results/, tools/ and the Docker contexts are relative to it

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
    # TLS_RESUME=0: no session resumption, every connection is a full handshake.
    if [[ "${TLS_RESUME:-1}" == 0 ]]; then GEN_ARGS+=(--no-resume); LABEL="$PROXY-tlsfull"; fi
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
. "$SCRIPTS/env.sh"
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
raw="$out/.raw"   # raw counters of the test in progress (snapshots before and after, the generator's output)
mkdir -p "$out" "$raw"
if [[ "$PROXY_H2" == 1 ]]; then TRANSPORT=h2
elif [[ "$PROXY_TLS" == 1 && "${TLS_RESUME:-1}" == 0 ]]; then TRANSPORT=tlsfull
elif [[ "$PROXY_TLS" == 1 ]]; then TRANSPORT=tls
else TRANSPORT=plain; fi
python3 tools/record.py init "$out" --proxy "$PROXY" --transport "$TRANSPORT" --matrix "$([[ "${MATRIX:-}" == ws ]] && echo ws || echo http)" \
    --host docker --auth "$AUTH" --repeat "$REPEAT" --workers "$WORKERS" \
    --param "access_log=$ACCESS_LOG" --param "ktls=$KTLS" --param "seccomp=${SECCOMP:-default}" --param "proxy_cpu=$PROXY_CPU" \
    --param "bench_cpu=$BENCH_CPU" --param "gen_threads=$GEN_THREADS" --param "up_threads=$UP_THREADS" --param "sink_threads=$SINK_THREADS"
echo "# $PROXY auth=$AUTH tls=$PROXY_TLS h2=$PROXY_H2 repeat=$REPEAT workers=$WORKERS proxy_cpu=$PROXY_CPU bench_cpu=$BENCH_CPU gen_threads=$GEN_THREADS $(date)"

# Raw snapshots, taken before and after each test and read by tools/record.py:
# the proxy's whole container (cgroup v2: multi-process DUTs such as Squid SMP
# workers included), the network counters of the proxy's namespace, the CPU of
# every process of the origin (nginx master + workers) and of the sink.
snap() { # <directory>
    mkdir -p "$1"
    docker compose exec -T proxy cat /sys/fs/cgroup/cpu.stat > "$1/proxy.cpu_stat" || : > "$1/proxy.cpu_stat"
    docker compose exec -T proxy cat /proc/net/snmp /proc/net/netstat > "$1/proxy.net" 2>/dev/null || : > "$1/proxy.net"
    docker compose exec -T upstream sh -c 'cat /proc/[0-9]*/stat 2>/dev/null' > "$1/up.stat" || : > "$1/up.stat"
    docker compose exec -T sink sh -c 'cat /proc/[0-9]*/stat 2>/dev/null' > "$1/sink.stat" || : > "$1/sink.stat"
}
# Memory of the proxy's processes (VmRSS: held after the load ended; VmHWM: the
# peak of the test, the proxy restarts before each test), after the run.
snap_status() { # <directory>
    docker compose exec -T proxy sh -c 'cat /proc/[0-9]*/status 2>/dev/null' > "$1/proxy.status" || : > "$1/proxy.status"
}

run_test() {
    local name="$1"; shift
    echo
    echo "### $name"
    docker compose restart proxy >/dev/null  # fresh state + fresh CPU counters
    sleep 0.5
    # Warmup in its own generator run, before the counters are sampled:
    # the proxy's CPU then covers the measured runs only (a warmup inside
    # the measured runs would add ~6 % to every figure).
    docker compose --profile generator run --rm -T generator \
        --warmup-only "${GEN_ARGS[@]}" "$@" >/dev/null 2>&1 || true
    snap "$raw/before"
    docker compose --profile generator run --rm -T generator \
        --repeat "$REPEAT" --no-warmup "${GEN_ARGS[@]}" "$@" > "$raw/gen.json" \
        || true  # no result (generator/proxy died): record.py reports it, the matrix goes on
    snap "$raw/after"; snap_status "$raw/after"
    record_add --generator "$raw/gen.json" --repeat "$REPEAT" --proxy-cpus "$PROXY_CPU" \
        --up-threads "$UP_THREADS" --sink-threads "$SINK_THREADS"
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
    local half=$(( (conns+1)/2 ))
    snap "$raw/before"
    for i in 1 2; do
        docker compose --profile generator run -d --name "holdrep-$i" \
            generator --repeat 1 --no-warmup "${GEN_ARGS[@]}" hold sink:9000 \
            --conns "$half" --duration "$duration" --ramp 4096 >/dev/null
    done
    for i in 1 2; do docker wait "holdrep-$i" >/dev/null; done
    snap "$raw/after"; snap_status "$raw/after"
    for i in 1 2; do
        docker logs "holdrep-$i" 2>/dev/null > "$raw/gen$i.json" || : > "$raw/gen$i.json"
        docker rm -f "holdrep-$i" >/dev/null 2>&1 || true
    done
    record_add --generator "$raw/gen1.json" --generator "$raw/gen2.json" --conns "$conns" --repeat 1 \
        --proxy-cpus "$PROXY_CPU" --up-threads "$UP_THREADS" --sink-threads "$SINK_THREADS"
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
. "$SCRIPTS/matrix.sh"
matrix_select_and_shuffle

for entry in "${shuffled[@]}"; do
    run_entry "$entry"
done

echo
echo "==> run in $out"
[[ "${INVALID:-0}" == 1 ]] && { echo "!! this run is INVALID: see the INVALID lines above (problems of each result are in $out/run.json)" >&2; exit 3; }
exit 0
