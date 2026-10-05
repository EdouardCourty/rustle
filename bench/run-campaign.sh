#!/usr/bin/env bash
# Full campaign: every DUT on every transport it supports (plain, then TLS
# and HTTP/2 for the DUTs whose Dockerfile declares them in its
# "# bench-ports:" line), same seed, one core for the proxy.
#
# Usage:
#   ./run-campaign.sh                      # auth on, all DUTs
#   AUTH=off PROXIES="rustle squid" ./run-campaign.sh
#
# Index of the runs: results/campaign-<ts>.txt ("<proxy> <mode> <results.txt>").
# Tables: tools/campaign-table.py results/campaign-<ts>.txt
set -uo pipefail
cd "$(dirname "$0")"

PROXIES="${PROXIES:-rustle 3proxy squid gost tinyproxy}"
AUTH="${AUTH:-on}"
export BENCH_SEED="${BENCH_SEED:-2004113323}"
mkdir -p results
index="results/campaign-$(date +%Y%m%d-%H%M%S).txt"
echo "# campaign proxies=[$PROXIES] auth=$AUTH seed=$BENCH_SEED $(date)" | tee "$index"

run() { # <proxy> <mode> [ENV=...]
    local p="$1" mode="$2"; shift 2
    echo "==> $p $mode" | tee -a "$index"
    env "$@" ./run-bench.sh "$p" "$AUTH" 2>&1 | tee /dev/stderr \
        | grep -a '^==> results in' | sed "s/^==> results in /$p $mode /" >> "$index"
}

for p in $PROXIES; do run "$p" plain PROXY_TLS=0 PROXY_H2=0; done
for p in $PROXIES; do
    grep -q '^# bench-ports:.*tls=' "dut/$p/Dockerfile" && run "$p" tls PROXY_TLS=1
done
for p in $PROXIES; do
    grep -q '^# bench-ports:.*h2=' "dut/$p/Dockerfile" && run "$p" h2 PROXY_H2=1
done
echo "==> done: $index"
