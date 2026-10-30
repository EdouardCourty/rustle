#!/usr/bin/env bash
# One command from a clean machine to the published artifacts:
#
#   HTTP/CONNECT campaign (5 proxies x plain/TLS/h2)  ->  WebSocket campaign (plain, TLS)
#   -> aggregation of the raw runs into data/<campaign>/benchmarks.json (the website's data)
#   -> validity listing -> SVG charts (docs/benchmarks/), README table and numbers, RESULTS.md tables
#
# Single host (Docker, Linux):      bench/scripts/run-all.sh
# Three machines (bench/REMOTE.md): REMOTE=1 GEN_HOST=... PROXY_HOST=... UP_HOST=... PROXY_CPUS=0 bench/scripts/run-all.sh
#
#   REPORT_ONLY=1  no campaign: aggregate and render from the newest manifests of results/
#   SKIP_WS=1      only the HTTP/CONNECT campaign
#   FLOOD_FROM=<manifest>  take the tunnel throughput row and chart from another campaign (a manifest
#                  written with ROLE=flood, run on one big host over loopback: the three-machine
#                  link caps tunnels)
#   CAMPAIGN=<id>  campaign folder under data/ (default: `latest` of data/index.json); a new campaign
#                  needs its folder with campaign.json and proxies.json first (data/README.md)
#   AUTH=on|off, PROXIES="...", BENCH_SEED, REPEAT: as run-campaign.sh
#
# The numbers are never typed: the raw runs (results/**/run.json) are listed in manifests
# (results/campaign-<ts>.json), tools/aggregate.py turns them into benchmarks.json with the rules of
# the benchmark (best clean level of the sweep, validity), tools/render.py writes everything else
# from benchmarks.json. The hardware, network, limits and the kTLS / multi-core runs come from the
# campaign's campaign.json: update it, and point its `sources` at the refreshed published/ copy,
# before committing.
#
# Nothing is published blindly: aggregate.py --report lists every point that is invalid,
# unsaturated or missing, and the script exits non-zero (the artifacts are still written, for a look).
set -uo pipefail
SCRIPTS="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPTS/.."   # the benchmark root: dut/, results/, tools/ and the Docker contexts are relative to it

ts=$(date +%Y%m%d-%H%M%S)
mkdir -p results
last() { ls -t "results/$1"-[0-9]*.json 2>/dev/null | head -1; }

if [[ "${REPORT_ONLY:-0}" != 1 ]]; then
    MANIFEST="results/campaign-$ts.json" "$SCRIPTS/run-campaign.sh" || echo "!! run-campaign.sh reported a failure; continuing with what exists" >&2
fi
main=$(last campaign)
[[ -n "$main" ]] || { echo "no campaign manifest produced" >&2; exit 2; }
sources=(--sources "$main")

if [[ "${SKIP_WS:-0}" != 1 ]]; then
    [[ "${REPORT_ONLY:-0}" == 1 ]] || MATRIX=ws MANIFEST="results/campaign-ws-$ts.json" "$SCRIPTS/run-campaign.sh" || echo "!! WebSocket campaign reported a failure; continuing" >&2
    ws=$(last campaign-ws)
    [[ -z "$ws" ]] || sources+=(--sources "$ws")
fi
[[ -z "${FLOOD_FROM:-}" ]] || sources+=(--sources "$FLOOD_FROM")
campaign=(${CAMPAIGN:+--campaign "$CAMPAIGN"})

status=0
report="results/report-$ts.txt"
python3 tools/aggregate.py ${campaign[@]+"${campaign[@]}"} "${sources[@]}" --report | tee "$report" || status=1
echo "==> report: $report"
python3 tools/aggregate.py ${campaign[@]+"${campaign[@]}"} "${sources[@]}" --write || status=1
python3 tools/render.py ${campaign[@]+"${campaign[@]}"} --write || status=1
((status == 0)) && echo "==> done, nothing to review" || echo "==> done: review the points above before committing" >&2
exit $status
