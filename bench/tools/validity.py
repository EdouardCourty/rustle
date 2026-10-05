#!/usr/bin/env python3
"""Is a campaign publishable? Lists, for every proxy and test, the best sweep
level when it is not clean:

    tools/validity.py <campaign index> [...]

- invalid: client errors, or fewer tunnels opened than asked;
- unsaturated: the proxy used < 90 % of its cores at the best level (the
  harness, a too small sweep or a latency-bound proxy set the number);
- harness saturated: origin or sink above 90 % of its threads.
Proxies absent from an index (no results at all) are listed as unsupported.
Exit code 1 when anything needs a look."""
import os, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from benchlib import load_index  # noqa: E402

if len(sys.argv) < 2:
    sys.exit(__doc__)
problems = 0
for idx in sys.argv[1:]:
    data = load_index(idx)
    wanted = set()
    for line in open(idx):
        p = line.split()
        if len(p) >= 3 and not line.startswith(("#", "==>")):
            wanted.add((p[1], "rustle" if p[0] == "myproxy" else p[0]))
    for tr, proxy in sorted(wanted):
        rows = data.get(tr, {}).get(proxy)
        if not rows:
            print(f"[{os.path.basename(idx)}] {proxy} ({tr}): no results (unsupported or failed): list it as unsupported, never as 0")
            problems += 1
            continue
        for test, r in sorted(rows.items()):
            at = f"@{r['conns_level']}" if r.get("conns_level") and not test.startswith("hold-") else ""
            if not r["valid"]:
                print(f"[{os.path.basename(idx)}] {proxy} ({tr}) {test}: INVALID (client errors or tunnels missing)")
                problems += 1
            elif r["util"] is None and not test.startswith("hold-"):
                print(f"[{os.path.basename(idx)}] {proxy} ({tr}) {test}: no saturation data (campaign made before the sweep and saturation checks): re-run it")
                problems += 1
            elif r["unsat"]:
                print(f"[{os.path.basename(idx)}] {proxy} ({tr}) {test} {at}: proxy only at {r['util']} of its cores: extend the sweep before publishing")
                problems += 1
            elif r["harness_sat"]:
                print(f"[{os.path.basename(idx)}] {proxy} ({tr}) {test} {at}: origin or sink saturated: add harness threads")
                problems += 1
# TLS parity: every proxy on the same version and cipher, and resumption
# actually on ("tls") or off ("tlsfull").
for idx in sys.argv[1:]:
    data = load_index(idx)
    for tr in ("tls", "tlsfull"):
        seen = {}
        for proxy, rows in data.get(tr, {}).items():
            for test, r in rows.items():
                if r.get("tls_negotiated"):
                    seen.setdefault(r["tls_negotiated"], set()).add(proxy)
                rs = r.get("tls_resumed")
                if rs is not None and tr == "tls" and test.endswith("-short") and rs < 0.5:
                    print(f"[{os.path.basename(idx)}] {proxy} (tls) {test}: only {rs:.0%} of the connections were resumed (does it issue session tickets?)")
                    problems += 1
                if rs is not None and tr == "tlsfull" and rs > 0.05:
                    print(f"[{os.path.basename(idx)}] {proxy} (tlsfull) {test}: {rs:.0%} resumed, expected none")
                    problems += 1
        if len(seen) > 1:
            detail = "; ".join(f"{k}: {', '.join(sorted(v))}" for k, v in sorted(seen.items()))
            print(f"[{os.path.basename(idx)}] {tr}: the proxies did not negotiate the same TLS version/cipher ({detail}): align them or say so next to the numbers")
            problems += 1
if problems:
    print(f"\n{problems} point(s) need attention before publishing.")
    sys.exit(1)
print("All best levels are clean and saturated.")
