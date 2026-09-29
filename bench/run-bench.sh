#!/usr/bin/env bash
# Benchmark one proxy (myproxy | 3proxy) inside Docker.
#
# Usage:
#   ./run-bench.sh [myproxy|3proxy] [on|off]
#   REPEAT=5 ./run-bench.sh myproxy on     # more repetitions
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

PROXY="${1:-myproxy}"
AUTH="${2:-on}"
REPEAT="${REPEAT:-3}"
HOLD_MAX_SINGLE=30000   # above this, split across 2 client replicas (ephemeral ports)

export PROXY_IMAGE="${PROXY}:bench"
export AUTH

# CPU pinning auto-détecté (partagé avec le Makefile).
. "$(dirname "$0")/env.sh"
echo "==> cpu pinning: proxy=$PROXY_CPU generator/upstream/sink=$BENCH_CPU (available: $ALL_CPUS)"

echo "==> building images (serialized: rustc peaks ~1-2 GB transiently)"
docker compose build generator >/dev/null
docker compose build upstream >/dev/null
docker build -q -t "$PROXY_IMAGE" -f "dut/$PROXY/Dockerfile" .. >/dev/null

echo "==> starting stack (proxy=$PROXY auth=$AUTH repeat=$REPEAT)"
docker compose up -d upstream sink proxy >/dev/null
trap 'docker compose down >/dev/null 2>&1' EXIT
sleep 1

out="results/${PROXY}/auth-${AUTH}/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$out"
: > "$out/results.txt"
echo "# $PROXY auth=$AUTH repeat=$REPEAT $(date)" | tee -a "$out/results.txt"

# CPU ticks (utime+stime, CLK_TCK=100) and RSS in MB of the proxy process.
cpu_ticks() { docker compose exec -T proxy cat /proc/1/stat | awk '{print $14+$15}'; }
rss_mb() { docker compose exec -T proxy cat /proc/1/statm | awk '{printf "%.1f", $2*4096/1048576}'; }
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
    local tick0 tick1 median cpu_s_per_run up0 up1 up_cpu_s_per_run
    tick0=$(cpu_ticks)
    up0=$(up_ticks)
    docker compose --profile generator run --rm -T generator \
        --repeat "$REPEAT" "$@" | tee /dev/stderr | grep '^RESULT median' > /tmp/opencode/median.txt
    tick1=$(cpu_ticks)
    up1=$(up_ticks)
    median=$(cat /tmp/opencode/median.txt)
    if [ -z "$median" ]; then
        echo "!! no RESULT median line for $name" | tee -a "$out/results.txt"
        return
    fi
    # CPU seconds attributable to one measured run (warmup amortized over REPEAT).
    cpu_s_per_run=$(awk -v a="$tick1" -v b="$tick0" -v n="$REPEAT" \
        'BEGIN{printf "%.3f", (a-b)/100/n}')
    up_cpu_s_per_run=$(awk -v a="$up1" -v b="$up0" -v n="$REPEAT" \
        'BEGIN{printf "%.2f", (a-b)/100/n}')
    echo "$median cpu_s_per_run=$cpu_s_per_run up_cpu_s_per_run=$up_cpu_s_per_run rss_mb=$(rss_mb)" | tee -a "$out/results.txt"
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
            generator --repeat 1 --no-warmup hold sink:9000 \
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
    echo "RESULT median mode=hold conns=$conns replicas=2 opened=$opened failed=$failed rate=$rate elapsed_s=$maxelapsed cpu_s_per_run=$cpu_s_per_run rss_mb=$(rss_mb)" | tee -a "$out/results.txt"
}

# --- Warmup réseau (avant toute mesure): montée des conntrack, caches
# DNS/kernel, fenêtres TCP autotunées — sans ça, les 1ers tests de la
# matrice absorbent la chauffe et la variance entre passes est ±30 %.
echo "### warmup-network"
docker compose --profile generator run --rm -T generator --repeat 1 --no-warmup \
    http http://upstream:8000/1mb --conns 16 --requests 200 >/dev/null 2>&1 || true
docker compose --profile generator run --rm -T generator --repeat 1 --no-warmup \
    flood sink:9000 --conns 8 --duration 3 >/dev/null 2>&1 || true

# --- Workloads, ordre randomisé à seed fixe (reproductible d'une passe à
# l'autre, mais supprime le biais d'ordre: le réseau n'est pas dans le même
# état après le hold-25k qu'après le 1kb). OVERRIDE=$SEED pour reproduire.
if [ -n "${BENCH_SEED:-}" ]; then SEED="$BENCH_SEED"; else SEED=$RANDOM$RANDOM; fi
echo "seed=$SEED" | tee -a "$out/results.txt"

declare -a TESTS=(
    # 1 KB keep-alive: conns raised 16->64 (M10.2-C). At 16 conns the
    # ceiling was the harness' own RTT (conns/latency), not the proxy.
    "run_test http-1kb http http://upstream:8000/1kb --conns 64 --duration 15"
    "run_test http-10kb http http://upstream:8000/10kb --conns 16 --duration 15"
    "run_test http-1mb http http://upstream:8000/1mb --conns 16 --duration 15"
    "run_test http-10mb http http://upstream:8000/10mb --conns 8 --duration 15"
    "run_test http-1kb-short http http://upstream:8000/1kb --conns 64 --duration 15 --no-keepalive"
    "run_test flood-8 flood sink:9000 --conns 8 --duration 10"
    "run_hold 1000 10"
    "run_hold 5000 10"
    "run_hold 10000 10"
    "run_hold 25000 10"
)
# BENCH_ONLY=<test-name>: run a single step (validate/iterate cheaply —
# a failed step is re-run alone instead of the whole matrix).
if [[ -n "${BENCH_ONLY:-}" ]]; then
    TESTS=()
    for t in "${shuffled-init:-}"; do :; done # no-op keeps shopt sane
    declare -a TESTS_ALL=(
        "run_test http-1kb http http://upstream:8000/1kb --conns 64 --duration 15"
        "run_test http-10kb http http://upstream:8000/10kb --conns 16 --duration 15"
        "run_test http-1mb http http://upstream:8000/1mb --conns 16 --duration 15"
        "run_test http-10mb http http://upstream:8000/10mb --conns 8 --duration 15"
        "run_test http-1kb-short http http://upstream:8000/1kb --conns 64 --duration 15 --no-keepalive"
        "run_test flood-8 flood sink:9000 --conns 8 --duration 10"
        "run_hold 1000 10"
        "run_hold 5000 10"
        "run_hold 10000 10"
        "run_hold 25000 10"
    )
    found=""
    for t in "${TESTS_ALL[@]}"; do
        if [[ "$t" == *" ${BENCH_ONLY} "* || "$t" == "run_${BENCH_ONLY} "* || "$t" == *" ${BENCH_ONLY} 10" ]]; then
            TESTS+=("$t")
            found=1
        fi
    done
    [[ -n "$found" ]] || { echo "no test matches BENCH_ONLY=${BENCH_ONLY}" >&2; exit 2; }
fi
# Fisher-Yates avec seed
shopt -s extglob
shuffled=(); tmp=("${TESTS[@]}")
n=${#tmp[@]}
for ((i=0; i<n; i++)); do
    # pseudo-random déterministe sur le seed
    SEED=$(( (SEED * 1103515245 + 12345) & 0x7fffffff ))
    j=$(( i + (SEED % (n - i)) ))
    shuffled[i]=${tmp[j]}
    tmp[j]=${tmp[i]}
done

for entry in "${shuffled[@]}"; do
    eval "$entry"
done

echo
echo "==> results in $out/results.txt"
