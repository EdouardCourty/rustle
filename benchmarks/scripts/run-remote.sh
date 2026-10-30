#!/usr/bin/env bash
# Benchmark one proxy across THREE machines, so that the load generator can
# no longer be the bottleneck:
#
#   GEN_HOST (load generator) --> PROXY_HOST (proxy under test) --> UP_HOST (origin + sink)
#
# Same matrix, seed, shuffle and run.json results as run-bench.sh (matrix.sh);
# only the plumbing differs: every host is driven over SSH, containers use
# the host network. Setup, sizing and sysctls: REMOTE.md.
#
# Usage:
#   GEN_HOST=a PROXY_HOST=b UP_HOST=c benchmarks/scripts/run-remote.sh rustle on
#   DRY_RUN=1 ... benchmarks/scripts/run-remote.sh rustle on        # print the commands only
#
# Environment:
#   GEN_HOST PROXY_HOST UP_HOST   ssh targets (required)
#   GEN_ADDR PROXY_ADDR UP_ADDR   addresses as seen by the other machines,
#                                 ideally the private network (default = *_HOST)
#   REMOTE_DIR                    repo checkout on every host (default ~/rustle)
#   PROXY_CPUS                    REQUIRED: cpuset of the proxy container (0 = one core, 0-3 = four);
#                                 WORKERS must equal the number of cores (same cores for every proxy)
#   WORKERS                       shards of the proxy (default 1; rustle, squid)
#   GEN_THREADS UP_THREADS SINK_THREADS   harness threads (default: sized from each host's CPUs)
#   REPEAT BENCH_SEED BENCH_ONLY ACCESS_LOG KTLS PROXY_TLS PROXY_H2   as run-bench.sh
#   MATRIX=ws                     WebSocket workloads instead of the HTTP/CONNECT matrix
#   TLS_RESUME=0 (with PROXY_TLS=1) every TLS connection is a full handshake
#   SKIP_BUILD=1                  images already built on every host
set -euo pipefail
SCRIPTS="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPTS/.."   # the benchmark root: dut/, results/, tools/ and the Docker contexts are relative to it

PROXY="${1:-rustle}"
AUTH="${2:-on}"
: "${GEN_HOST:?set GEN_HOST}" "${PROXY_HOST:?set PROXY_HOST}" "${UP_HOST:?set UP_HOST}"
# Every proxy must get the same cores: unset, the 1-core proxies (rustle, Squid) would
# be compared with thread-per-connection ones (3proxy, Tinyproxy) and Go (gost) using
# the whole machine. 0 = one core, 0-3 = four cores (and WORKERS=4).
: "${PROXY_CPUS:?set PROXY_CPUS (e.g. 0 for the 1-core campaign, 0-3 for four cores)}"
GEN_ADDR="${GEN_ADDR:-$GEN_HOST}"; PROXY_ADDR="${PROXY_ADDR:-$PROXY_HOST}"; UP_ADDR="${UP_ADDR:-$UP_HOST}"
REMOTE_DIR="${REMOTE_DIR:-~/rustle}"
REPEAT="${REPEAT:-3}"
WORKERS="${WORKERS:-1}"
ACCESS_LOG="${ACCESS_LOG:-off}"
KTLS="${KTLS:-false}"
PROXY_TLS="${PROXY_TLS:-0}"; PROXY_H2="${PROXY_H2:-0}"
ncpus=$(python3 tools/record.py cpus "$PROXY_CPUS")
[[ "$WORKERS" == "$ncpus" ]] || { echo "WORKERS=$WORKERS but PROXY_CPUS=$PROXY_CPUS is $ncpus core(s): set WORKERS=$ncpus" >&2; exit 2; }
DRY_RUN="${DRY_RUN:-0}"
export BENCH_SEED="${BENCH_SEED:-2004113323}"
ULIMIT=(--ulimit nofile=524288:524288)

# Run a command on a host. Dry run: print it instead.
on() { local host="$1"; shift
    if [[ "$DRY_RUN" == 1 ]]; then echo "[$host] $*" >&2; return 0; fi
    ssh -o BatchMode=yes "$host" "$*"
}

# Harness threads sized to each machine unless given: the generator gets half the
# threads of its host (it is the heaviest part of the harness), the origin and the
# sink a quarter each. The saturation flags in the results say if that was enough.
threads_for() { # <host> <divisor> <min> <max>
    local n; n=$(on "$1" nproc 2>/dev/null | tr -dc '0-9'); n=${n:-8}
    local t=$(( n / $2 )); (( t < $3 )) && t=$3; (( t > $4 )) && t=$4; echo "$t"
}
GEN_THREADS="${GEN_THREADS:-$(threads_for "$GEN_HOST" 2 2 24)}"
UP_THREADS="${UP_THREADS:-$(threads_for "$UP_HOST" 4 2 12)}"
SINK_THREADS="${SINK_THREADS:-$(threads_for "$UP_HOST" 4 2 12)}"

_port() { sed -n "s/^# bench-ports:.*$1=\([0-9]*\).*/\1/p" "dut/$PROXY/Dockerfile"; }
proxy_port=8080; GEN_ARGS=(--threads "$GEN_THREADS"); LABEL="$PROXY"
if [[ "$PROXY_H2" == 1 ]]; then
    proxy_port=$(_port h2); LABEL="$PROXY-h2"; GEN_ARGS+=(--proxy-h2)
    [[ -n "$proxy_port" ]] || { echo "$PROXY has no HTTP/2 proxy listener" >&2; exit 2; }
elif [[ "$PROXY_TLS" == 1 ]]; then
    proxy_port=$(_port tls); LABEL="$PROXY-tls"; GEN_ARGS+=(--proxy-tls)
    # TLS_RESUME=0: no session resumption, every connection is a full handshake.
    if [[ "${TLS_RESUME:-1}" == 0 ]]; then GEN_ARGS+=(--no-resume); LABEL="$PROXY-tlsfull"; fi
    [[ -n "$proxy_port" ]] || { echo "$PROXY has no HTTPS proxy listener" >&2; exit 2; }
fi
if [[ "${MATRIX:-}" == ws ]]; then
    [[ "$PROXY_H2" == 1 ]] && { echo "MATRIX=ws: WebSocket upgrades are HTTP/1.1 only (no PROXY_H2)" >&2; exit 2; }
    LABEL="$LABEL-ws"
fi
GEN_ARGS+=(--proxy "$PROXY_ADDR:$proxy_port")

# --- Images (built where they run) -----------------------------------------
if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
    echo "==> building images"
    on "$GEN_HOST"   "cd $REMOTE_DIR/benchmarks && docker build -q -t bench-client generator" >/dev/null
    on "$UP_HOST"    "cd $REMOTE_DIR/benchmarks && docker build -q -t bench-client generator" >/dev/null
    on "$PROXY_HOST" "cd $REMOTE_DIR/benchmarks && docker build -q -t $PROXY:bench -f dut/$PROXY/Dockerfile .." >/dev/null
fi

cleanup() {
    on "$PROXY_HOST" "docker rm -f proxy" >/dev/null 2>&1 || true
    on "$UP_HOST" "docker rm -f upstream sink" >/dev/null 2>&1 || true
}
trap cleanup EXIT

start_proxy() {
    on "$PROXY_HOST" "docker rm -f proxy" >/dev/null 2>&1 || true
    on "$PROXY_HOST" "docker run -d --name proxy --network host --pids-limit -1 --cpuset-cpus $PROXY_CPUS ${ULIMIT[*]} \
        -e AUTH=$AUTH -e WORKERS=$WORKERS -e ACCESS_LOG=$ACCESS_LOG -e KTLS=$KTLS -e SOCK_BUF=${SOCK_BUF:-1048576} $PROXY:bench" >/dev/null
}
echo "==> starting origin and sink on $UP_HOST, proxy ($PROXY) on $PROXY_HOST"
on "$UP_HOST" "docker rm -f upstream sink" >/dev/null 2>&1 || true
on "$UP_HOST" "docker run -d --name upstream --network host ${ULIMIT[*]} --entrypoint bench-client bench-client \
    --threads $UP_THREADS upstream 0.0.0.0:8000" >/dev/null
on "$UP_HOST" "docker run -d --name sink --network host ${ULIMIT[*]} --entrypoint bench-client bench-client \
    --threads $SINK_THREADS sink 0.0.0.0:9000" >/dev/null
start_proxy
sleep 2

out="results/${LABEL}/remote-auth-${AUTH}/$(date +%Y%m%d-%H%M%S)"
raw="$out/.raw"   # raw counters of the test in progress (snapshots before and after, the generator's output)
mkdir -p "$out" "$raw"
if [[ "$PROXY_H2" == 1 ]]; then TRANSPORT=h2
elif [[ "$PROXY_TLS" == 1 && "${TLS_RESUME:-1}" == 0 ]]; then TRANSPORT=tlsfull
elif [[ "$PROXY_TLS" == 1 ]]; then TRANSPORT=tls
else TRANSPORT=plain; fi
python3 tools/record.py init "$out" --proxy "$PROXY" --transport "$TRANSPORT" --matrix "$([[ "${MATRIX:-}" == ws ]] && echo ws || echo http)" \
    --host remote --auth "$AUTH" --repeat "$REPEAT" --workers "$WORKERS" \
    --param "proxy_cpus=$PROXY_CPUS" --param "gen_threads=$GEN_THREADS" --param "up_threads=$UP_THREADS" \
    --param "sink_threads=$SINK_THREADS" --param "gen=$GEN_HOST" --param "proxy=$PROXY_HOST" --param "up=$UP_HOST"
echo "# $PROXY remote auth=$AUTH tls=$PROXY_TLS h2=$PROXY_H2 repeat=$REPEAT workers=$WORKERS proxy_cpus=$PROXY_CPUS gen_threads=$GEN_THREADS up_threads=$UP_THREADS sink_threads=$SINK_THREADS gen=$GEN_HOST proxy=$PROXY_HOST up=$UP_HOST $(date)"

# --- Raw snapshots on the machines --------------------------------------------
# Taken before and after each test and read by tools/record.py: the proxy's whole
# container (cgroup v2: multi-process DUTs included), the proxy host's network
# counters (host network: the whole machine), the CPU of every process of the origin
# and of the sink, and, after the run, the memory of the proxy's processes.
snap() { # <directory>
    mkdir -p "$1"
    on "$PROXY_HOST" "docker exec proxy cat /sys/fs/cgroup/cpu.stat" > "$1/proxy.cpu_stat" || : > "$1/proxy.cpu_stat"
    on "$PROXY_HOST" "cat /proc/net/snmp /proc/net/netstat" > "$1/proxy.net" || : > "$1/proxy.net"
    on "$UP_HOST" "docker exec upstream sh -c 'cat /proc/[0-9]*/stat 2>/dev/null'" > "$1/up.stat" || : > "$1/up.stat"
    on "$UP_HOST" "docker exec sink sh -c 'cat /proc/[0-9]*/stat 2>/dev/null'" > "$1/sink.stat" || : > "$1/sink.stat"
}
snap_status() { # <directory>
    on "$PROXY_HOST" "docker exec proxy sh -c 'cat /proc/[0-9]*/status 2>/dev/null'" > "$1/proxy.status" || : > "$1/proxy.status"
}

gen() { on "$GEN_HOST" "docker run --rm --network host ${ULIMIT[*]} bench-client $*"; }

run_test() {
    local name="$1"; shift
    echo; echo "### $name"
    start_proxy; sleep 1   # fresh state + fresh CPU counters
    gen --warmup-only "${GEN_ARGS[@]}" "$@" >/dev/null 2>&1 || true
    snap "$raw/before"
    gen --repeat "$REPEAT" --no-warmup "${GEN_ARGS[@]}" "$@" > "$raw/gen.json" || true  # no result: record.py reports it, the matrix goes on
    snap "$raw/after"; snap_status "$raw/after"
    # a generator at its limit measures itself, not the proxy: record.py flags the run (--gen-threads)
    record_add --generator "$raw/gen.json" --repeat "$REPEAT" --proxy-cpus "$PROXY_CPUS" \
        --up-threads "$UP_THREADS" --sink-threads "$SINK_THREADS" --gen-threads "$GEN_THREADS"
}

# A dedicated generator host has its own ephemeral ports: no replica split.
run_hold() {
    local conns="$1" duration="${2:-10}"
    run_test "hold-${conns}" hold "$UP_ADDR:9000" --conns "$conns" --duration "$duration" --ramp 4096
}

# Workloads point at the origin and sink by address (matrix.sh).
export UP_HTTP="$UP_ADDR:8000" UP_SINK="$UP_ADDR:9000"
. "$SCRIPTS/matrix.sh"

echo "### warmup-network"
gen --repeat 1 --no-warmup "${GEN_ARGS[@]}" http "http://$UP_HTTP/1mb" --conns 16 --requests 200 >/dev/null 2>&1 || true
gen --repeat 1 --no-warmup "${GEN_ARGS[@]}" flood "$UP_SINK" --conns 8 --duration 3 >/dev/null 2>&1 || true

matrix_select_and_shuffle
for entry in "${shuffled[@]}"; do run_entry "$entry"; done

echo; echo "==> run in $out"
[[ "${INVALID:-0}" == 1 ]] && { echo "!! this run is INVALID: see the INVALID lines above (problems of each result are in $out/run.json)" >&2; exit 3; }
exit 0
