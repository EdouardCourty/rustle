#!/usr/bin/env bash
# Benchmark one proxy (myproxy | 3proxy) inside Docker.
#
# Usage:
#   ./run-bench.sh [myproxy|3proxy] [on|off]
#
# Results are appended to bench/results/<proxy>/auth-<on|off>/summary.txt
set -euo pipefail
cd "$(dirname "$0")"

PROXY="${1:-myproxy}"
AUTH="${2:-on}"

export PROXY_IMAGE="${PROXY}:bench"
export AUTH

echo "==> building images"
docker compose build upstream sink client
if [ "$PROXY" = "myproxy" ]; then
    docker build -q -t myproxy:bench -f proxies/myproxy/Dockerfile ..
else
    docker build -q -t 3proxy:bench -f proxies/3proxy/Dockerfile ..
fi

echo "==> starting stack ($PROXY, auth=$AUTH)"
docker compose up -d upstream sink proxy
trap 'docker compose down' EXIT
sleep 1

out="results/${PROXY}/auth-${AUTH}/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$out"

run() {
    echo
    echo "### $*"
    docker compose --profile client run --rm -T client "$@" | tee -a "$out/summary.txt"
}

# A. HTTP small / medium / large
run http http://upstream:8000/1kb  --conns 16 --requests 200000
run http http://upstream:8000/10kb --conns 16 --requests 100000
run http http://upstream:8000/1mb  --conns 16 --requests 10000

# C. Short connections (1 request per connection)
run http http://upstream:8000/1kb --conns 64 --requests 100000 --no-keepalive

# B. CONNECT tunnel throughput
run flood sink:9000 --conns 8 --duration 10

# E. Concurrency curve (held tunnels)
for n in 1000 10000 50000; do
    run hold sink:9000 --conns "$n" --duration 10 --ramp 4096
done

echo
echo "==> results in $out/summary.txt"
