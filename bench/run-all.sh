#!/usr/bin/env bash
# One command from a clean machine to the published artifacts:
#
#   HTTP/CONNECT campaign (5 proxies x plain/TLS/h2)  ->  WebSocket campaign (plain, TLS)
#   -> Markdown report -> validity check -> SVG charts (docs/benchmarks/) -> README table
#
# Single host (Docker, Linux):      ./run-all.sh
# Three machines (bench/REMOTE.md): REMOTE=1 GEN_HOST=... PROXY_HOST=... UP_HOST=... PROXY_CPUS=0 ./run-all.sh
#
#   REPORT_ONLY=1  no campaign: rebuild report, charts and README table from the newest indexes
#   SKIP_WS=1      only the HTTP/CONNECT campaign
#   NO_README=1    do not touch README.md (charts and report only)
#   AUTH=on|off, PROXIES="...", BENCH_SEED, REPEAT: as run-campaign.sh
#
# Nothing is published blindly: tools/validity.py runs before the charts and
# the README table, prints every point that is invalid, unsaturated or missing,
# and the script exits non-zero (the artifacts are still written, for a look).
set -uo pipefail
cd "$(dirname "$0")"

ts=$(date +%Y%m%d-%H%M%S)
report="results/report-$ts.md"
mkdir -p results
last_index() { ls -t "results/$1"-[0-9]*.txt 2>/dev/null | head -1; }

if [[ "${REPORT_ONLY:-0}" != 1 ]]; then
    ./run-campaign.sh || echo "!! run-campaign.sh reported a failure; continuing with what exists" >&2
fi
http_idx=$(last_index campaign)
[[ -n "$http_idx" ]] || { echo "no campaign index produced" >&2; exit 2; }

ws_idx=""
if [[ "${SKIP_WS:-0}" != 1 ]]; then
    [[ "${REPORT_ONLY:-0}" == 1 ]] || MATRIX=ws ./run-campaign.sh || echo "!! WebSocket campaign reported a failure; continuing" >&2
    ws_idx=$(last_index campaign-ws)
fi

{
    echo "# Benchmark report $ts"
    echo
    echo "HTTP/CONNECT campaign: \`$http_idx\`${ws_idx:+, WebSocket campaign: \`$ws_idx\`}"
    echo
    ./tools/campaign-table.py "$http_idx"
    [[ -n "$ws_idx" ]] && ./tools/campaign-table.py "$ws_idx"
} > "$report"
echo "==> report: $report"

status=0
./tools/validity.py "$http_idx" ${ws_idx:+"$ws_idx"} | tee -a "$report" || status=1

python3 tools/charts.py "$http_idx" >/dev/null && echo "==> charts: docs/benchmarks/ (HTTP/CONNECT)"
[[ -n "$ws_idx" ]] && python3 tools/charts.py "$ws_idx" >/dev/null && echo "==> charts: docs/benchmarks/ (WebSocket)"
if [[ "${NO_README:-0}" != 1 ]]; then
    ./tools/readme-table.py "$http_idx" ${ws_idx:+"$ws_idx"} --write ../README.md || status=1
fi
((status == 0)) && echo "==> done, nothing to review" || echo "==> done: review the points above before committing" >&2
exit $status
