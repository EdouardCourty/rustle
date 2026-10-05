#!/usr/bin/env bash
# Benchmark one proxy across THREE machines, so that the load generator can
# no longer be the bottleneck:
#
#   GEN_HOST (load generator) --> PROXY_HOST (proxy under test) --> UP_HOST (origin + sink)
#
# Same matrix, seed, shuffle and RESULT lines as run-bench.sh (matrix.sh);
# only the plumbing differs: every host is driven over SSH, containers use
# the host network. Setup, sizing and sysctls: REMOTE.md.
#
# Usage:
#   GEN_HOST=a PROXY_HOST=b UP_HOST=c ./run-remote.sh rustle on
#   DRY_RUN=1 ... ./run-remote.sh rustle on        # print the commands only
#
# Environment:
#   GEN_HOST PROXY_HOST UP_HOST   ssh targets (required)
#   GEN_ADDR PROXY_ADDR UP_ADDR   addresses as seen by the other machines,
#                                 ideally the private network (default = *_HOST)
#   REMOTE_DIR                    repo checkout on every host (default ~/rustle)
#   PROXY_CPUS                    cpuset of the proxy container, e.g. 0-3 (default: all)
#   WORKERS                       shards of the proxy (default 1; rustle, squid)
#   GEN_THREADS UP_THREADS SINK_THREADS   harness threads (default: 4 / 4 / 4)
#   REPEAT BENCH_SEED BENCH_ONLY ACCESS_LOG KTLS PROXY_TLS PROXY_H2   as run-bench.sh
#   MATRIX=ws                     WebSocket workloads instead of the HTTP/CONNECT matrix
#   SKIP_BUILD=1                  images already built on every host
set -euo pipefail
cd "$(dirname "$0")"

PROXY="${1:-rustle}"
AUTH="${2:-on}"
: "${GEN_HOST:?set GEN_HOST}" "${PROXY_HOST:?set PROXY_HOST}" "${UP_HOST:?set UP_HOST}"
GEN_ADDR="${GEN_ADDR:-$GEN_HOST}"; PROXY_ADDR="${PROXY_ADDR:-$PROXY_HOST}"; UP_ADDR="${UP_ADDR:-$UP_HOST}"
REMOTE_DIR="${REMOTE_DIR:-~/rustle}"
REPEAT="${REPEAT:-3}"
WORKERS="${WORKERS:-1}"
ACCESS_LOG="${ACCESS_LOG:-off}"
KTLS="${KTLS:-false}"
GEN_THREADS="${GEN_THREADS:-4}"; UP_THREADS="${UP_THREADS:-4}"; SINK_THREADS="${SINK_THREADS:-4}"
PROXY_TLS="${PROXY_TLS:-0}"; PROXY_H2="${PROXY_H2:-0}"
DRY_RUN="${DRY_RUN:-0}"
export BENCH_SEED="${BENCH_SEED:-2004113323}"
ULIMIT=(--ulimit nofile=524288:524288)

# Run a command on a host. Dry run: print it instead.
on() { local host="$1"; shift
    if [[ "$DRY_RUN" == 1 ]]; then echo "[$host] $*" >&2; return 0; fi
    ssh -o BatchMode=yes "$host" "$*"
}

_port() { sed -n "s/^# bench-ports:.*$1=\([0-9]*\).*/\1/p" "dut/$PROXY/Dockerfile"; }
proxy_port=8080; GEN_ARGS=(--threads "$GEN_THREADS"); LABEL="$PROXY"
if [[ "$PROXY_H2" == 1 ]]; then
    proxy_port=$(_port h2); LABEL="$PROXY-h2"; GEN_ARGS+=(--proxy-h2)
    [[ -n "$proxy_port" ]] || { echo "$PROXY has no HTTP/2 proxy listener" >&2; exit 2; }
elif [[ "$PROXY_TLS" == 1 ]]; then
    proxy_port=$(_port tls); LABEL="$PROXY-tls"; GEN_ARGS+=(--proxy-tls)
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
    on "$GEN_HOST"   "cd $REMOTE_DIR/bench && docker build -q -t bench-client generator" >/dev/null
    on "$UP_HOST"    "cd $REMOTE_DIR/bench && docker build -q -t bench-client generator" >/dev/null
    on "$PROXY_HOST" "cd $REMOTE_DIR/bench && docker build -q -t $PROXY:bench -f dut/$PROXY/Dockerfile .." >/dev/null
fi

cleanup() {
    on "$PROXY_HOST" "docker rm -f proxy" >/dev/null 2>&1 || true
    on "$UP_HOST" "docker rm -f upstream sink" >/dev/null 2>&1 || true
}
trap cleanup EXIT

start_proxy() {
    on "$PROXY_HOST" "docker rm -f proxy" >/dev/null 2>&1 || true
    on "$PROXY_HOST" "docker run -d --name proxy --network host ${PROXY_CPUS:+--cpuset-cpus $PROXY_CPUS} ${ULIMIT[*]} \
        -e AUTH=$AUTH -e WORKERS=$WORKERS -e ACCESS_LOG=$ACCESS_LOG -e KTLS=$KTLS $PROXY:bench" >/dev/null
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
mkdir -p "$out"; : > "$out/results.txt"
echo "# $PROXY remote auth=$AUTH tls=$PROXY_TLS h2=$PROXY_H2 repeat=$REPEAT workers=$WORKERS proxy_cpus=${PROXY_CPUS:-all} gen_threads=$GEN_THREADS up_threads=$UP_THREADS sink_threads=$SINK_THREADS gen=$GEN_HOST proxy=$PROXY_HOST up=$UP_HOST $(date)" | tee -a "$out/results.txt"

# --- Probes on the proxy host ----------------------------------------------
# Whole container (cgroup v2): multi-process DUTs included. Unit: 1/100 s.
cpu_ticks() { on "$PROXY_HOST" "docker exec proxy cat /sys/fs/cgroup/cpu.stat" | awk '/^usage_usec/{printf "%d", $2/10000}'; }
_sum_status() { on "$PROXY_HOST" "docker exec proxy sh -c 'cat /proc/[0-9]*/status 2>/dev/null'" \
    | awk -v k="$1:" '$1=="Name:"{n=$2} $1==k && n!="sh" && n!="cat"{s+=$2} END{printf "%.1f", s/1024}'; }
up_ticks() { on "$UP_HOST" "docker exec upstream sh -c 'cat /proc/[0-9]*/stat 2>/dev/null | awk \"{s+=\\\$14+\\\$15} END{print s+0}\"'" ; }
# The proxy host's TCP counters (host network: the whole machine).
tcp_counters() {
    on "$PROXY_HOST" "cat /proc/net/snmp /proc/net/netstat" | awk '
        /^(Tcp|TcpExt):/ { if (!(($1) in hdr)) { for (i=2;i<=NF;i++) k[$1,i]=$i; hdr[$1]=1 }
                           else { for (i=2;i<=NF;i++) v[k[$1,i]]=$i } }
        END { printf "%d %d %d %d %d %d", v["OutSegs"], v["ActiveOpens"], v["RetransSegs"],
              v["DelayedACKs"], v["ListenOverflows"], v["ListenDrops"] }'
}
tcp_delta() {
    awk -v a="$1" -v b="$2" -v n="$3" 'BEGIN { split(a,x," "); split(b,y," ");
        printf "segs_out_per_run=%d active_opens_per_run=%d retrans_per_run=%d delayed_acks_per_run=%d listen_overflows=%d listen_drops=%d",
            (y[1]-x[1])/n, (y[2]-x[2])/n, (y[3]-x[3])/n, (y[4]-x[4])/n, y[5]-x[5], y[6]-x[6] }'
}

gen() { on "$GEN_HOST" "docker run --rm --network host ${ULIMIT[*]} bench-client $*"; }

# A generator at its limit measures itself, not the proxy: flag the run.
check_generator() { # <RESULT line>
    awk -v t="$GEN_THREADS" '{ for (i=1;i<=NF;i++) { split($i,a,"=");
        if (a[1]=="gen_cpu_s_per_run") g=a[2]; if (a[1]=="elapsed_s") e=a[2] }
        if (e>0 && g/(e*t) > 0.7) printf "gen_saturated=1 (generator at %.0f%% of its threads: raise GEN_THREADS or the host size)", 100*g/(e*t) }' <<<"$1"
}

run_test() {
    local name="$1"; shift
    echo; echo "### $name"
    start_proxy; sleep 1   # fresh state + fresh CPU counters
    gen --warmup-only "${GEN_ARGS[@]}" "$@" >/dev/null 2>&1 || true
    local tick0 tick1 up0 up1 tcp0 tcp1 median
    tick0=$(cpu_ticks); up0=$(up_ticks); tcp0=$(tcp_counters)
    gen --repeat "$REPEAT" --no-warmup "${GEN_ARGS[@]}" "$@" | tee /dev/stderr | grep '^RESULT median' > "$out/.median.txt" || true
    tick1=$(cpu_ticks); up1=$(up_ticks); tcp1=$(tcp_counters)
    median=$(cat "$out/.median.txt")
    if [[ -z "$median" ]]; then echo "!! no RESULT median line for $name" | tee -a "$out/results.txt"; return; fi
    local cpu up_cpu
    cpu=$(awk -v a="$tick1" -v b="$tick0" -v n="$REPEAT" 'BEGIN{printf "%.3f", (a-b)/100/n}')
    up_cpu=$(awk -v a="$up1" -v b="$up0" -v n="$REPEAT" 'BEGIN{printf "%.2f", (a-b)/100/n}')
    echo "$median cpu_s_per_run=$cpu up_cpu_s_per_run=$up_cpu rss_mb=$(_sum_status VmRSS) peak_rss_mb=$(_sum_status VmHWM) $(tcp_delta "$tcp0" "$tcp1" "$REPEAT") $(check_generator "$median")" | tee -a "$out/results.txt"
}

# A dedicated generator host has its own ephemeral ports: no replica split.
run_hold() {
    local conns="$1" duration="${2:-10}"
    run_test "hold-${conns}" hold "$UP_ADDR:9000" --conns "$conns" --duration "$duration" --ramp 4096
}

# Workloads point at the origin and sink by address (matrix.sh).
export UP_HTTP="$UP_ADDR:8000" UP_SINK="$UP_ADDR:9000"
. ./matrix.sh

echo "### warmup-network"
gen --repeat 1 --no-warmup "${GEN_ARGS[@]}" http "http://$UP_HTTP/1mb" --conns 16 --requests 200 >/dev/null 2>&1 || true
gen --repeat 1 --no-warmup "${GEN_ARGS[@]}" flood "$UP_SINK" --conns 8 --duration 3 >/dev/null 2>&1 || true

matrix_select_and_shuffle
for entry in "${shuffled[@]}"; do eval "$entry"; done

echo; echo "==> results in $out/results.txt"
