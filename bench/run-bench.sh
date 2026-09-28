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
# Layout (2 cores): proxy pinned to core 0; client/upstream/sink on core 1.
# Results: bench/results/<proxy>/auth-<on|off>/<timestamp>/results.txt
set -euo pipefail
cd "$(dirname "$0")"

PROXY="${1:-myproxy}"
AUTH="${2:-on}"
REPEAT="${REPEAT:-3}"
HOLD_MAX_SINGLE=30000   # above this, split across 2 client replicas (ephemeral ports)

export PROXY_IMAGE="${PROXY}:bench"
export AUTH

# CPU pinning: give the proxy its own core, everything else another one.
# Auto-detect from the cgroup cpuset (e.g. containers exposing "0,2").
ALL_CPUS=$(cat /sys/fs/cgroup/cpuset.cpus.effective 2>/dev/null || grep Cpus_allowed_list /proc/self/status | awk '{print $2}')
PROXY_CPU=$(awk -F',' '{split($1,a,"-"); print a[1]}' <<<"$ALL_CPUS")
BENCH_CPU=$(awk -F',' -v skip="$PROXY_CPU" '{
    n=split($0,t,","); out="";
    for (i=1;i<=n;i++) { split(t[i],a,"-"); s=a[1]; e=(a[2]==""?s:a[2]);
        for (c=s;c<=e;c++) if (c+0!=skip+0) { out=out (out==""?"":",") c } }
    print (out==""?skip:out)
}' <<<"$ALL_CPUS")
export PROXY_CPU BENCH_CPU
echo "==> cpu pinning: proxy=$PROXY_CPU client/upstream/sink=$BENCH_CPU (available: $ALL_CPUS)"

echo "==> building images (serialized: rustc peaks ~1-2 GB transiently)"
docker compose build client >/dev/null
docker compose build upstream >/dev/null
docker build -q -t "$PROXY_IMAGE" -f "proxies/$PROXY/Dockerfile" .. >/dev/null

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

run_test() {
    local name="$1"; shift
    echo
    echo "### $name"
    docker compose restart proxy >/dev/null  # fresh state + fresh CPU counters
    sleep 0.5
    local tick0 tick1 median cpu_s_per_run
    tick0=$(cpu_ticks)
    docker compose --profile client run --rm -T client \
        --repeat "$REPEAT" "$@" | tee /dev/stderr | grep '^RESULT median' > /tmp/opencode/median.txt
    tick1=$(cpu_ticks)
    median=$(cat /tmp/opencode/median.txt)
    if [ -z "$median" ]; then
        echo "!! no RESULT median line for $name" | tee -a "$out/results.txt"
        return
    fi
    # CPU seconds attributable to one measured run (warmup amortized over REPEAT).
    cpu_s_per_run=$(awk -v a="$tick1" -v b="$tick0" -v n="$REPEAT" \
        'BEGIN{printf "%.3f", (a-b)/100/n}')
    echo "$median cpu_s_per_run=$cpu_s_per_run rss_mb=$(rss_mb)" | tee -a "$out/results.txt"
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
        docker compose --profile client run -d --name "holdrep-$i" \
            client --repeat 1 --no-warmup hold sink:9000 \
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

# --- A. HTTP small / medium / large (keep-alive, also covers D) ---
run_test http-1kb  http http://upstream:8000/1kb  --conns 16 --requests 100000
run_test http-10kb http http://upstream:8000/10kb --conns 16 --requests 50000
run_test http-1mb  http http://upstream:8000/1mb  --conns 16 --requests 5000
run_test http-10mb http http://upstream:8000/10mb --conns 8  --requests 300

# --- C. Short connections (1 request per connection) ---
run_test http-1kb-short http http://upstream:8000/1kb --conns 64 --requests 50000 --no-keepalive

# --- B. CONNECT tunnel throughput ---
run_test flood-8 flood sink:9000 --conns 8 --duration 10

# --- E. Concurrency curve (held tunnels) ---
run_hold 1000
run_hold 5000
run_hold 10000
run_hold 25000

echo
echo "==> results in $out/results.txt"
