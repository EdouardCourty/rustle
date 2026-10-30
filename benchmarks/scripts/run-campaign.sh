#!/usr/bin/env bash
# Full campaign: every DUT on every transport it supports (plain, then TLS
# and HTTP/2 for the DUTs whose Dockerfile declares them in its
# "# bench-ports:" line), same seed, one core for the proxy.
#
# Usage:
#   benchmarks/scripts/run-campaign.sh                      # auth on, all DUTs
#   AUTH=off PROXIES="rustle squid" benchmarks/scripts/run-campaign.sh
#
# REMOTE=1: the same campaign on three machines (run-remote.sh; GEN_HOST, PROXY_HOST,
# UP_HOST and the *_ADDR variables of benchmarks/documentation/REMOTE.md).
# MATRIX=ws: the WebSocket matrix (plain and TLS only), manifest results/campaign-ws-<ts>.json.
# Manifest of the runs: results/campaign-<ts>.json (MANIFEST=<file> to choose it), the `runs` of
# campaign.json's `sources`: one entry per run directory, {role, proxy, transport, run}. The role is
# `main` (`ws` for the WebSocket matrix); ROLE=ktls (a KTLS=true run of rustle) or ROLE=flood (the
# tunnel test from another campaign, see run-all.sh) label a fragment for the other roles.
# Numbers: tools/aggregate.py --sources <manifest>, then tools/render.py (make bench-publish).
set -uo pipefail
SCRIPTS="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPTS/.."   # the benchmark root: dut/, results/, tools/ and the Docker contexts are relative to it

PROXIES="${PROXIES:-rustle 3proxy squid gost tinyproxy}"
AUTH="${AUTH:-on}"
export BENCH_SEED="${BENCH_SEED:-2004113323}"
mkdir -p results
if [[ "${REMOTE:-0}" == 1 ]]; then
    RUNNER=remote
    # Same cores for every proxy (one core unless told otherwise).
    export PROXY_CPUS="${PROXY_CPUS:-0}" WORKERS="${WORKERS:-1}"
else
    RUNNER=bench
fi
if [[ "${MATRIX:-}" == ws ]]; then pfx=campaign-ws; ROLE="${ROLE:-ws}"; else pfx=campaign; ROLE="${ROLE:-main}"; fi
manifest="${MANIFEST:-results/$pfx-$(date +%Y%m%d-%H%M%S).json}"
echo "# campaign proxies=[$PROXIES] auth=$AUTH seed=$BENCH_SEED role=$ROLE $(date)"

run() { # <proxy> <mode> [ENV=...]
    local p="$1" mode="$2"; shift 2
    echo "==> $p $mode"
    env "$@" "$SCRIPTS/run-${RUNNER}.sh" "$p" "$AUTH" 2>&1 | tee /dev/stderr | while IFS= read -r line; do
        case "$line" in
            "==> run in "*) python3 tools/record.py manifest "$manifest" --role "$ROLE" --proxy "$p" --transport "$mode" --run "${line#==> run in }" ;;
        esac
    done
}

# MODES="plain tls tlsfull h2" by default; e.g. MODES=plain for a tunnel-only run.
MODES="${MODES:-plain tls tlsfull h2}"
want() { [[ " $MODES " == *" $1 "* ]]; }

if want plain; then
    for p in $PROXIES; do run "$p" plain PROXY_TLS=0 PROXY_H2=0; done
fi
if want tls; then
    for p in $PROXIES; do
        grep -q '^# bench-ports:.*tls=' "dut/$p/Dockerfile" && run "$p" tls PROXY_TLS=1
    done
fi
# Same short TLS connections without session resumption (a client with no
# session cache pays the full handshake, certificate signature included).
if want tlsfull && [[ "${MATRIX:-}" != ws ]]; then
    for p in $PROXIES; do
        grep -q '^# bench-ports:.*tls=' "dut/$p/Dockerfile" \
            && run "$p" tlsfull PROXY_TLS=1 TLS_RESUME=0 BENCH_ONLY=http-1kb-short
    done
fi
if want h2 && [[ "${MATRIX:-}" != ws ]]; then
    for p in $PROXIES; do
        grep -q '^# bench-ports:.*h2=' "dut/$p/Dockerfile" && run "$p" h2 PROXY_H2=1
    done
fi
echo "==> done: $manifest"
