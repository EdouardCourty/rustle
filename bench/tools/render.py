#!/usr/bin/env python3
"""Everything published from the numbers is a pure function of bench/data/<latest>/benchmarks.json (and
the campaign's campaign.json / proxies.json): the SVG charts of docs/benchmarks/, the summary table of
the README and the generated tables of bench/RESULTS.md. Nothing is typed by hand.

    tools/render.py            check that the committed files are what the data gives (exit 1 otherwise)
    tools/render.py --write    rewrite them
    tools/render.py --campaign <id>

The README table sits between `<!-- bench:table:start -->` and `<!-- bench:table:end -->`; RESULTS.md has
`<!-- bench:<name>:start -->` / `<!-- bench:<name>:end -->` blocks of generated tables and `{{ ... }}`
placeholders in its prose (see results_md.py)."""
import json, os, sys
from xml.sax.saxutils import escape

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import aggregate  # noqa: E402
from aggregate import ROOT, campaign_dir, latest_id, load_json  # noqa: E402

ONE = "1 core · auth on · best of the concurrency sweep · median of 3"
RPS = lambda v: f"{v:,.0f} req/s"
CSS = """
.bg{fill:#fff}.t{fill:#1f2328;font:600 15px -apple-system,Segoe UI,Helvetica,Arial,sans-serif}
.s{fill:#656d76;font:12px -apple-system,Segoe UI,Helvetica,Arial,sans-serif}
.l{fill:#1f2328;font:13px -apple-system,Segoe UI,Helvetica,Arial,sans-serif}
.v{fill:#1f2328;font:600 13px -apple-system,Segoe UI,Helvetica,Arial,sans-serif}
.o{fill:#8c959f}.r{fill:#e5532d}
@media (prefers-color-scheme:dark){.bg{fill:#0d1117}.t,.l,.v{fill:#e6edf3}.s{fill:#8b949e}.o{fill:#6e7681}.r{fill:#ff7b54}}
"""

# chart file -> (test id, title, subtitle, value format); lower is better when the test's direction says so
CHARTS = {
    "plain-1kb.svg": ("plain-1kb", "HTTP, 1 KB responses", f"Requests per second, higher is better · {ONE}", RPS),
    "plain-1mb.svg": ("plain-1mb", "HTTP, 1 MB responses", f"Requests per second, higher is better · {ONE}", RPS),
    "plain-cpu.svg": ("plain-cpu", "CPU cost per request (1 KB)",
                      f"Microseconds of proxy CPU per request, lower is better · {ONE}", lambda v: f"{v:.0f} µs"),
    "plain-flood.svg": ("plain-flood", "CONNECT tunnels, bulk transfer",
                        f"Gbit/s through 8 tunnels, higher is better · {ONE}", lambda v: f"{v:.1f} Gbit/s"),
    "plain-ram.svg": ("plain-ram-25000", "Memory with 25,000 open tunnels", f"Peak RSS, lower is better · {ONE}",
                      lambda v: f"{v:,.0f} MB"),
    "ws-64b-rps.svg": ("ws-64b-rps", "WebSocket, 64-byte messages",
                       f"Messages per second (64 connections, ping-pong), higher is better · {ONE}",
                       lambda v: f"{v:,.0f} msg/s"),
    "ws-64b-latency.svg": ("ws-64b-latency", "WebSocket round trip, 64-byte messages",
                           f"p99 latency in milliseconds, lower is better · {ONE}", lambda v: f"{v:.2f} ms"),
    "ws-cpu.svg": ("ws-cpu", "CPU cost per WebSocket message",
                   f"Microseconds of proxy CPU per 64-byte message, lower is better · {ONE}", lambda v: f"{v:.1f} µs"),
    "ws-64kb-gbps.svg": ("ws-64kb-gbps", "WebSocket, 64 KB messages",
                         f"Gbit/s of payload (8 connections, 8 frames in flight), higher is better · {ONE}",
                         lambda v: f"{v:.1f} Gbit/s"),
    "tls-short.svg": ("tls-short", "HTTPS proxy, a new connection per request",
                      f"Requests per second with TLS session resumption, higher is better · {ONE}", RPS),
    "tls-short-full.svg": ("tls-short-full", "HTTPS proxy, full handshake every time",
                           f"Requests per second without session resumption (certificate signature each time), higher is better · {ONE}",
                           RPS),
    "tls-1kb.svg": ("tls-1kb", "HTTPS proxy (TLS), 1 KB responses", f"Requests per second, higher is better · {ONE}", RPS),
    "h2-1kb.svg": ("h2-1kb", "HTTP/2 proxy, 1 KB responses", f"Requests per second, higher is better · {ONE}", RPS),
}


def chart(title, sub, items, fmt, lower_better):
    """One horizontal bar chart; items: [(proxy, value)], best first, rustle highlighted."""
    items = sorted([i for i in items if i[1] is not None], key=lambda i: i[1], reverse=not lower_better)
    if not items:
        return None
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
    return "\n".join(s) + "\n"


def charts(bench):
    tests = {t["id"]: t for t in bench["tests"]}
    out = {}
    for fname, (tid, title, sub, fmt) in CHARTS.items():
        t = tests.get(tid)
        if t is None:
            continue
        svg = chart(title, sub, list(t["values"].items()), fmt, t["direction"] == "lower")
        if svg:
            out[os.path.join("docs", "benchmarks", fname)] = svg
    return out


# ---- README table -----------------------------------------------------------

def fmt_ram(mb):
    return f"{mb / 1024:.1f} GB" if mb >= 1024 else f"{mb:.0f} MB"


README_ROWS = [  # (label, test id, formatter)
    ("1 KB, req/s", "plain-1kb", lambda v: f"{v:,.0f}"),
    ("1 MB, req/s", "plain-1mb", lambda v: f"{v:,.0f}"),
    ("10 MB, req/s", "plain-10mb", lambda v: f"{v:,.0f}"),
    ("Short connections, req/s", "plain-short", lambda v: f"{v:,.0f}"),
    ("Tunnels, Gbit/s", "plain-flood", lambda v: f"{v:.1f}"),
    ("25,000 tunnels, RAM", "plain-ram-25000", fmt_ram),
    ("HTTPS, new connection, resumed, req/s", "tls-short", lambda v: f"{v:,.0f}"),
    ("HTTPS, new connection, full handshake, req/s", "tls-short-full", lambda v: f"{v:,.0f}"),
    ("WebSocket 64 B, msg/s", "ws-64b-rps", lambda v: f"{v:,.0f}"),
    ("WebSocket 64 KB, Gbit/s", "ws-64kb-gbps", lambda v: f"{v:.1f}"),
    ("WebSocket handshakes/s", "ws-short", lambda v: f"{v:,.0f}"),
]


def readme_table(bench, proxies):
    """The summary table of README.md: rows with a number, the best value of each in bold, — where there is none."""
    tests = {t["id"]: t for t in bench["tests"]}
    ids = [p["id"] for p in proxies["proxies"]]
    names = {p["id"]: p["name"] for p in proxies["proxies"]}
    lines = ["| 1 core | " + " | ".join(names[i] for i in ids) + " |", "|---|" + "---|" * len(ids)]
    for label, tid, fmt in README_ROWS:
        t = tests.get(tid)
        if t is None or all(v is None for v in t["values"].values()):
            continue
        if "loopback" in t["conditions"]:
            label += " (single host, loopback)"
        present = [v for v in t["values"].values() if v is not None]
        best = max(present) if t["direction"] == "higher" else min(present)
        cells = ["—" if t["values"][i] is None else (f"**{fmt(t['values'][i])}**" if t["values"][i] == best else fmt(t["values"][i])) for i in ids]
        lines.append(f"| {label} | " + " | ".join(cells) + " |")
    return "\n".join(lines)


def replace_block(text, name, body):
    a, b = f"<!-- bench:{name}:start -->", f"<!-- bench:{name}:end -->"
    if a not in text or b not in text:
        raise SystemExit(f"render: markers {a} / {b} not found")
    i, j = text.index(a) + len(a), text.index(b)
    return text[:i] + "\n\n" + body + "\n\n" + text[j:]


# ---- everything -------------------------------------------------------------

def render_all(cid):
    """{path relative to the repository: new content} for every generated file."""
    d = campaign_dir(cid)
    bench = load_json(os.path.join(d, "benchmarks.json"))
    proxies = load_json(os.path.join(d, "proxies.json"))
    campaign = load_json(os.path.join(d, "campaign.json"))
    out = charts(bench)
    readme = open(os.path.join(ROOT, "README.md")).read()
    import results_md
    out["README.md"] = results_md.expand_text(replace_block(readme, "table", readme_table(bench, proxies)), bench, proxies, campaign)
    results = open(os.path.join(ROOT, "bench", "RESULTS.md")).read()
    out["bench/RESULTS.md"] = results_md.render(results, bench, proxies, campaign)
    return out


def main(argv):
    write, cid, i = False, None, 0
    while i < len(argv):
        if argv[i] == "--write":
            write = True
        elif argv[i] == "--campaign" and i + 1 < len(argv):
            cid, i = argv[i + 1], i + 1
        else:
            sys.exit(__doc__)
        i += 1
    cid = cid or latest_id()
    stale = []
    for path, text in render_all(cid).items():
        full = os.path.join(ROOT, path)
        current = open(full).read() if os.path.isfile(full) else None
        if current != text:
            stale.append(path)
            if write:
                with open(full, "w") as f:
                    f.write(text)
    if write:
        print(f"render: {len(stale)} file(s) rewritten" + (": " + ", ".join(stale) if stale else ""))
    elif stale:
        print("render: out of date with the benchmark data (run `make data`): " + ", ".join(stale), file=sys.stderr)
        sys.exit(1)
    else:
        print("render: charts, README table and RESULTS.md tables up to date")


if __name__ == "__main__":
    main(sys.argv[1:])
