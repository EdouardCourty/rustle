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
if problems:
    print(f"\n{problems} point(s) need attention before publishing.")
    sys.exit(1)
print("All best levels are clean and saturated.")
