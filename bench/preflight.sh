#!/usr/bin/env bash
# Checks (and with APPLY=1 sets) everything that must be identical on the three
# machines before a campaign, measures the links, and writes the result to
# results/preflight-<ts>.txt (keep it next to the numbers you publish).
#
#   GEN_HOST=a PROXY_HOST=b UP_HOST=c PROXY_CPUS=0 ./preflight.sh
#   APPLY=1 ...     also keep the proxy cores to the proxy: hyperthread siblings
#                   offline, NIC interrupts moved elsewhere (proxy host, through sudo)
#   DRY_RUN=1 ...   print the ssh commands only
#   GEN_ADDR PROXY_ADDR UP_ADDR   private addresses (default: the host names)
#   REMOTE_DIR      repository checkout on every host (default ~/rustle)
#   MIN_GBIT        link speed below which the run is refused (default 8)
# Machines are prepared once with remote/bootstrap.sh.
set -euo pipefail
cd "$(dirname "$0")"
: "${GEN_HOST:?}" "${PROXY_HOST:?}" "${UP_HOST:?}" "${PROXY_CPUS:?set PROXY_CPUS, e.g. 0 or 0-3}"
GEN_ADDR="${GEN_ADDR:-$GEN_HOST}"; PROXY_ADDR="${PROXY_ADDR:-$PROXY_HOST}"; UP_ADDR="${UP_ADDR:-$UP_HOST}"
REMOTE_DIR="${REMOTE_DIR:-~/rustle}"; MIN_GBIT="${MIN_GBIT:-8}"; DRY_RUN="${DRY_RUN:-0}"
out="results/preflight-$(date +%Y%m%d-%H%M%S).txt"; mkdir -p results

on() { local host="$1"; shift
    if [[ "$DRY_RUN" == 1 ]]; then echo "[$host] $*" >&2; return 0; fi
    ssh -o BatchMode=yes "$host" "$@"
}
feed() { local host="$1" file="$2"; shift 2    # run a local script on a host
    if [[ "$DRY_RUN" == 1 ]]; then echo "[$host] ${FEED_SUDO:+sudo }bash -s -- $* < $file" >&2; return 0; fi
    ssh -o BatchMode=yes "$host" "${FEED_SUDO:+sudo }bash -s -- $*" < "$file"
}

tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
for role in GEN PROXY UP; do
    host_var="${role}_HOST"; host="${!host_var}"
    feed "$host" remote/probe.sh "$REMOTE_DIR" > "$tmp/$role" || { echo "!! $role ($host): probe failed" >&2; exit 2; }
done

status=0
if [[ "$DRY_RUN" != 1 ]]; then
{
    echo "# preflight $(date) local-commit=$(git rev-parse --short HEAD) proxy_cpus=$PROXY_CPUS"
    python3 - "$tmp" "$MIN_GBIT" <<'PY'
import sys, os
tmp, min_gbit = sys.argv[1], float(sys.argv[2])
roles = ["GEN", "PROXY", "UP"]
data = {}
for r in roles:
    d = {}
    for line in open(os.path.join(tmp, r)):
        if "=" in line:
            k, v = line.rstrip("\n").split("=", 1); d[k] = v.strip()
    data[r] = d
keys = sorted({k for d in data.values() for k in d})
must_match = lambda k: k.startswith("sysctl.") or k in ("governor", "mtu", "offload", "kernel", "docker", "git", "iface")
bad = 0
print(f"{'':36} {'generator':>22} {'proxy':>22} {'origin':>22}")
for k in keys:
    vals = [data[r].get(k, "?") for r in roles]
    flag = ""
    if must_match(k) and len(set(vals)) > 1:
        flag = "  <-- DIFFERS"; bad += 1
    print(f"{k:36} {vals[0][:22]:>22} {vals[1][:22]:>22} {vals[2][:22]:>22}{flag}")
for r in roles:
    d = data[r]
    if d.get("governor") not in ("performance", "n/a"):
        print(f"!! {r}: CPU governor is {d.get('governor')}, expected performance (remote/bootstrap.sh)"); bad += 1
    if d.get("irqbalance") == "active":
        print(f"!! {r}: irqbalance is running (remote/bootstrap.sh stops it)"); bad += 1
    if d.get("docker") in ("missing", ""):
        print(f"!! {r}: Docker missing"); bad += 1
    if d.get("git") == "n/a" or d.get("git_dirty") not in ("0",):
        print(f"!! {r}: repository missing or with uncommitted changes (git={d.get('git')}, dirty={d.get('git_dirty')})"); bad += 1
    try:
        if d.get("link_mbit") not in (None, "n/a") and float(d["link_mbit"]) / 1000 < min_gbit:
            print(f"!! {r}: advertised link {d['link_mbit']} Mbit/s is below MIN_GBIT"); bad += 1
    except ValueError:
        pass
print(f"\nproblems: {bad}")
sys.exit(1 if bad else 0)
PY
} | tee "$out" || status=1

# Link speed actually measured (the advertised speed of a cloud NIC means little).
for spec in "GEN_HOST|$PROXY_HOST|$PROXY_ADDR|generator->proxy" "PROXY_HOST|$UP_HOST|$UP_ADDR|proxy->origin"; do
    IFS='|' read -r srcvar dsthost dstaddr label <<<"$spec"
    on "$dsthost" "pkill iperf3 || true; iperf3 -s -D" >/dev/null 2>&1 || true; sleep 1
    g=$(on "${!srcvar}" "iperf3 -c $dstaddr -P 4 -t 5 -J 2>/dev/null" \
        | python3 -c 'import json,sys; print(round(json.load(sys.stdin)["end"]["sum_received"]["bits_per_second"]/1e9,1))' 2>/dev/null || echo "?")
    on "$dsthost" "pkill iperf3 || true" >/dev/null 2>&1 || true
    echo "iperf3 $label: $g Gbit/s" | tee -a "$out"
    if [[ "$g" == "?" ]] || awk -v g="$g" -v m="$MIN_GBIT" 'BEGIN{exit !(g+0 < m+0)}'; then
        echo "!! $label: link not measured or below MIN_GBIT=$MIN_GBIT" | tee -a "$out"; status=1
    fi
done
fi

if [[ "${APPLY:-0}" == 1 ]]; then
    FEED_SUDO=1 feed "$PROXY_HOST" remote/apply-proxy-host.sh "$PROXY_CPUS" | { [[ "$DRY_RUN" == 1 ]] && cat || tee -a "$out"; } || status=1
fi

[[ "$DRY_RUN" == 1 ]] && { echo "dry run: nothing executed"; exit 0; }
((status == 0)) && echo "PREFLIGHT OK -> $out" || { echo "PREFLIGHT FAILED -> $out" >&2; exit 1; }
