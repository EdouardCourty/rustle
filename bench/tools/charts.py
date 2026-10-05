#!/usr/bin/env python3
"""SVG bar charts for the README from a campaign index (run-campaign.sh):

    tools/charts.py results/campaign-<ts>.txt [OUT_DIR]   (default: ../docs/benchmarks)

One horizontal bar chart per metric, rustle highlighted, stdlib only. Colors
adapt to the viewer's light/dark theme. Same result parsing as
campaign-table.py."""
import os, sys
from xml.sax.saxutils import escape

idx = sys.argv[1]
out = sys.argv[2] if len(sys.argv) > 2 else os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "docs", "benchmarks")
base = os.path.dirname(os.path.dirname(os.path.abspath(idx)))  # bench/
os.makedirs(out, exist_ok=True)

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from benchlib import load_index  # noqa: E402

data = load_index(idx)

CSS = """
.bg{fill:#fff}.t{fill:#1f2328;font:600 15px -apple-system,Segoe UI,Helvetica,Arial,sans-serif}
.s{fill:#656d76;font:12px -apple-system,Segoe UI,Helvetica,Arial,sans-serif}
.l{fill:#1f2328;font:13px -apple-system,Segoe UI,Helvetica,Arial,sans-serif}
.v{fill:#1f2328;font:600 13px -apple-system,Segoe UI,Helvetica,Arial,sans-serif}
.o{fill:#8c959f}.r{fill:#e5532d}
@media (prefers-color-scheme:dark){.bg{fill:#0d1117}.t,.l,.v{fill:#e6edf3}.s{fill:#8b949e}.o{fill:#6e7681}.r{fill:#ff7b54}}
"""


def chart(fname, title, sub, items, fmt, lower_better=False):
    """items: [(proxy, value)]; missing values dropped. Best first."""
    items = sorted([i for i in items if i[1] == i[1]], key=lambda i: i[1], reverse=not lower_better)
    if not items:
        return
    W, LX, BX, BW, RH, TOP = 760, 24, 120, 520, 30, 62
    H = TOP + RH * len(items) + 16
    mx = max(v for _, v in items) or 1
    s = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="{escape(title)}">',
         f"<style>{CSS}</style>", f'<rect class="bg" width="{W}" height="{H}" rx="8"/>',
         f'<text class="t" x="{LX}" y="28">{escape(title)}</text>', f'<text class="s" x="{LX}" y="47">{escape(sub)}</text>']
    for n, (name, v) in enumerate(items):
        y = TOP + n * RH
        w = max(2, BW * v / mx)
        cls = "r" if name == "rustle" else "o"
        s.append(f'<text class="l" x="{LX}" y="{y + 17}">{escape(name)}</text>')
        s.append(f'<rect class="{cls}" x="{BX}" y="{y + 3}" width="{w:.1f}" height="20" rx="3"/>')
        s.append(f'<text class="v" x="{BX + w + 8:.1f}" y="{y + 18}">{escape(fmt(v))}</text>')
    s.append("</svg>")
    open(os.path.join(out, fname), "w").write("\n".join(s) + "\n")
    print("wrote", fname)


def col(tr, test, key):
    """(proxy, value) for the proxies that have a clean result for `test`
    (a run with client errors or missing tunnels is not charted)."""
    out = []
    for p, rows in data.get(tr, {}).items():
        r = rows.get(test)
        out.append((p, r[key] if r and r.get("valid") and key in r else float("nan")))
    return out


one = "1 core · auth on · best of the concurrency sweep · median of 3"
rps = lambda v: f"{v:,.0f} req/s"
chart("plain-1kb.svg", "HTTP, 1 KB responses", f"Requests per second, higher is better · {one}", col("plain", "http-1kb", "rps"), rps)
chart("plain-1mb.svg", "HTTP, 1 MB responses", f"Requests per second, higher is better · {one}", col("plain", "http-1mb", "rps"), rps)
chart("plain-cpu.svg", "CPU cost per request (1 KB)", f"Microseconds of proxy CPU per request, lower is better · {one}",
      col("plain", "http-1kb", "us"), lambda v: f"{v:.0f} µs", True)
chart("plain-flood.svg", "CONNECT tunnels, bulk transfer", f"Gbit/s through 8 tunnels, higher is better · {one}",
      col("plain", "flood", "gbps"), lambda v: f"{v:.1f} Gbit/s")
chart("plain-ram.svg", "Memory with 25,000 open tunnels", f"Peak RSS, lower is better · {one}",
      col("plain", "hold-25000", "mb"), lambda v: f"{v:,.0f} MB", True)
# WebSocket campaign (MATRIX=ws): proxies without Upgrade support have no row
# and are simply absent from the charts.
chart("ws-64b-rps.svg", "WebSocket, 64-byte messages", f"Messages per second (64 connections, ping-pong), higher is better · {one}",
      col("plain", "ws-64b", "rps"), lambda v: f"{v:,.0f} msg/s")
chart("ws-64b-latency.svg", "WebSocket round trip, 64-byte messages", f"p99 latency in milliseconds, lower is better · {one}",
      col("plain", "ws-64b", "p99"), lambda v: f"{v:.2f} ms", True)
chart("ws-cpu.svg", "CPU cost per WebSocket message", f"Microseconds of proxy CPU per 64-byte message, lower is better · {one}",
      col("plain", "ws-64b", "us"), lambda v: f"{v:.1f} µs", True)
chart("ws-64kb-gbps.svg", "WebSocket, 64 KB messages", f"Gbit/s of payload (8 connections, 8 frames in flight), higher is better · {one}",
      col("plain", "ws-64kb", "gbps"), lambda v: f"{v:.1f} Gbit/s")
chart("tls-1kb.svg", "HTTPS proxy (TLS), 1 KB responses", f"Requests per second, higher is better · {one}", col("tls", "http-1kb", "rps"), rps)
chart("h2-1kb.svg", "HTTP/2 proxy, 1 KB responses", f"Requests per second, higher is better · {one}", col("h2", "http-1kb", "rps"), rps)
