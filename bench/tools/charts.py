#!/usr/bin/env python3
"""SVG bar charts for the README from a campaign index (run-campaign.sh):

    tools/charts.py results/campaign-<ts>.txt [OUT_DIR]   (default: ../docs/benchmarks)

One horizontal bar chart per metric, rustle highlighted, stdlib only. Colors
adapt to the viewer's light/dark theme. Same result parsing as
campaign-table.py."""
import os, re, sys
from xml.sax.saxutils import escape

idx = sys.argv[1]
out = sys.argv[2] if len(sys.argv) > 2 else os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "docs", "benchmarks")
base = os.path.dirname(os.path.dirname(os.path.abspath(idx)))  # bench/
os.makedirs(out, exist_ok=True)

NAMES = {"myproxy": "rustle", "rustle": "rustle"}


def load(path):
    rows = {}
    for line in open(path, errors="replace"):
        if not line.startswith("RESULT median"):
            continue
        kv = dict(re.findall(r"(\w+)=(\S*)", line))
        m = re.search(r" (?<!gen_)(?<!up_)cpu_s_per_run=([0-9.]+)", line)
        cpu = float(m.group(1)) if m else float("nan")
        if kv["mode"] == "http":
            name = "http-" + kv["url"].rsplit("/", 1)[1] + ("" if kv["keepalive"] == "true" else "-short")
            rps = float(kv["rps"])
            rows[name] = {"rps": rps, "us": cpu / (rps * float(kv["elapsed_s"])) * 1e6}
        elif kv["mode"] == "flood":
            rows["flood"] = {"gbps": float(kv["gbps"])}
        else:
            rows["hold-" + kv["conns"]] = {"opened": float(kv["opened"]), "conns": float(kv["conns"]),
                                           "mb": float(kv.get("peak_rss_mb", "nan"))}
    return rows


data = {}  # transport -> proxy -> rows
for line in open(idx):
    if line.startswith("#") or line.startswith("==>"):
        continue
    p = line.split()
    if len(p) >= 3 and os.path.exists(os.path.join(base, p[2])):
        data.setdefault(p[1], {})[NAMES.get(p[0], p[0])] = load(os.path.join(base, p[2]))

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
    return [(p, r.get(test, {}).get(key, float("nan"))) for p, r in data.get(tr, {}).items()]


one = "1 core · auth on · median of 3 · seed 2004113323"
rps = lambda v: f"{v:,.0f} req/s"
chart("plain-1kb.svg", "HTTP, 1 KB responses", f"Requests per second, higher is better · {one}", col("plain", "http-1kb", "rps"), rps)
chart("plain-1mb.svg", "HTTP, 1 MB responses", f"Requests per second, higher is better · {one}", col("plain", "http-1mb", "rps"), rps)
chart("plain-cpu.svg", "CPU cost per request (1 KB)", f"Microseconds of proxy CPU per request, lower is better · {one}",
      col("plain", "http-1kb", "us"), lambda v: f"{v:.0f} µs", True)
chart("plain-flood.svg", "CONNECT tunnels, bulk transfer", f"Gbit/s through 8 tunnels, higher is better · {one}",
      col("plain", "flood", "gbps"), lambda v: f"{v:.1f} Gbit/s")
chart("plain-ram.svg", "Memory with 25,000 open tunnels", f"Peak RSS, lower is better · {one}",
      col("plain", "hold-25000", "mb"), lambda v: f"{v:,.0f} MB", True)
chart("tls-1kb.svg", "HTTPS proxy (TLS), 1 KB responses", f"Requests per second, higher is better · {one}", col("tls", "http-1kb", "rps"), rps)
chart("h2-1kb.svg", "HTTP/2 proxy, 1 KB responses", f"Requests per second, higher is better · {one}", col("h2", "http-1kb", "rps"), rps)
