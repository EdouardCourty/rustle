#!/usr/bin/env python3
"""README summary table from campaign indexes, written between the markers

    <!-- bench:table:start -->  ...  <!-- bench:table:end -->

    tools/readme-table.py <campaign index> [<ws index>] [--write README.md]

Without --write the table is printed. Rows use the best clean sweep level of
each proxy; a missing or invalid result shows "—" (never a number). Rustle is
the first column; the best value of each row is in bold."""
import os, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from benchlib import load_index  # noqa: E402

args = [a for a in sys.argv[1:] if not a.startswith("--")]
write = None
if "--write" in sys.argv:
    write = sys.argv[sys.argv.index("--write") + 1]
    args = [a for a in args if a != write]
if not args:
    sys.exit(__doc__)

ORDER = ["rustle", "3proxy", "squid", "gost", "tinyproxy"]
LABEL = {"squid": "Squid", "gost": "gost", "tinyproxy": "Tinyproxy"}


def fmt_ram(mb):
    return f"{mb / 1024:.1f} GB" if mb >= 1024 else f"{mb:.0f} MB"


# (label, test, key, higher is better, formatter, needs a clean result)
ROWS = [
    ("1 KB, req/s", "http-1kb", "rps", True, lambda v: f"{v:,.0f}"),
    ("1 MB, req/s", "http-1mb", "rps", True, lambda v: f"{v:,.0f}"),
    ("10 MB, req/s", "http-10mb", "rps", True, lambda v: f"{v:,.0f}"),
    ("Short connections, req/s", "http-1kb-short", "rps", True, lambda v: f"{v:,.0f}"),
    ("Tunnels, Gbit/s", "flood", "gbps", True, lambda v: f"{v:.1f}"),
    ("25,000 tunnels, RAM", "hold-25000", "mb", False, fmt_ram),
]
WS_ROWS = [
    ("WebSocket 64 B, msg/s", "ws-64b", "rps", True, lambda v: f"{v:,.0f}"),
    ("WebSocket 64 KB, Gbit/s", "ws-64kb", "gbps", True, lambda v: f"{v:.1f}"),
    ("WebSocket handshakes/s", "ws-short", "rps", True, lambda v: f"{v:,.0f}"),
]

plain = load_index(args[0]).get("plain", {})
ws = load_index(args[1]).get("plain", {}) if len(args) > 1 else {}
procs = [p for p in ORDER if p in plain or p in ws]
if "rustle" not in procs:
    sys.exit("no rustle results in the index")


def cell_values(src, test, key):
    out = {}
    for p in procs:
        r = src.get(p, {}).get(test)
        ok = r and r.get("valid") and key in r and r[key] == r[key]
        if test.startswith("hold-") and r:
            ok = ok and r["opened"] >= r["conns"]
        out[p] = r[key] if ok else None
    return out


lines = ["| 1 core | " + " | ".join(LABEL.get(p, p) for p in procs) + " |", "|---|" + "---|" * len(procs)]
for label, test, key, higher, fmt in ROWS + (WS_ROWS if ws else []):
    src = ws if test.startswith("ws-") else plain
    vals = cell_values(src, test, key)
    present = [v for v in vals.values() if v is not None]
    if not present:
        continue
    best = max(present) if higher else min(present)
    cells = []
    for p in procs:
        v = vals[p]
        cells.append("—" if v is None else (f"**{fmt(v)}**" if v == best else fmt(v)))
    lines.append(f"| {label} | " + " | ".join(cells) + " |")
table = "\n".join(lines)

if not write:
    print(table)
    sys.exit(0)
text = open(write).read()
a, b = "<!-- bench:table:start -->", "<!-- bench:table:end -->"
if a not in text or b not in text:
    sys.exit(f"{write}: markers {a} / {b} not found")
i, j = text.index(a) + len(a), text.index(b)
open(write, "w").write(text[:i] + "\n\n" + table + "\n\n" + text[j:])
print(f"table written to {write}")
