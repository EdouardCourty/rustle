#!/usr/bin/env python3
"""Markdown tables from a campaign index (run-campaign.sh):

    tools/campaign-table.py results/campaign-<ts>.txt

One table per transport (plain / tls / h2); rows = tests, columns = DUTs.
Cell: throughput and proxy CPU per unit (http: µs/req, flood: ms/GB, ws:
msg/s with p99 and µs/msg, or Gbps and ms/GB for large frames);
holds: tunnels opened / asked and peak RSS. A missing RESULT (DUT died)
shows as "—"."""
import re, sys, os

idx = sys.argv[1]
base = os.path.dirname(os.path.dirname(os.path.abspath(idx)))  # bench/
cols = []
for line in open(idx):
    if line.startswith("#") or line.startswith("==>"):
        continue
    parts = line.split()
    if len(parts) >= 3:
        cols.append((f"{parts[0]} {parts[1]}", os.path.join(base, parts[2])))

def load(p):
    """Best valid sweep level of each test (rows are RESULT lines of the same
    test at several --conns; a level with client errors only counts when no
    level is clean)."""
    pts = {}  # name -> [(valid, metric, cell)]
    for line in open(p, errors="replace"):
        if not line.startswith("RESULT median"):
            continue
        kv = dict(re.findall(r"(\w+)=(\S*)", line))
        # cpu_s_per_run appears 3x (gen_, proxy, up_): findall keeps last;
        # take the proxy one explicitly.
        m = re.search(r" (?<!gen_)(?<!up_)cpu_s_per_run=([0-9.]+)", line)
        cpu = float(m.group(1)) if m else float("nan")
        mode = kv["mode"]
        err = kv.get("err_kinds", "")
        at = f" @{kv['conns']}" if "conns" in kv and mode != "hold" else ""
        if mode == "http":
            name = "http-" + kv["url"].rsplit("/", 1)[1] + ("" if kv["keepalive"] == "true" else "-short")
            thr = float(kv["rps"]); per = cpu / (thr * float(kv["elapsed_s"])) * 1e6 if thr else float("nan")
            cell = f"{thr:,.0f} rps {per:.0f}µs"
        elif mode == "ws":
            size = int(kv["size"])
            name = "ws-short" if kv["short"] == "true" else "ws-" + (f"{size}b" if size < 1024 else f"{size // 1024}kb")
            thr = float(kv["rps"]); el = float(kv["elapsed_s"])
            per = cpu / (thr * el) * 1e6 if thr else float("nan")
            if size >= 1024:
                gb = float(kv["mbps"]) * el / 1024
                thr = float(kv["mbps"])
                cell = f"{float(kv['mbps']) * 8 / 1000:.1f} Gbps {cpu / gb * 1e3 if gb else float('nan'):.0f}ms/GB"
            else:
                cell = f"{thr:,.0f} msg/s p99 {float(kv['p99_ms']):.2f}ms {per:.0f}µs"
        elif mode == "flood":
            name = "flood"; thr = float(kv["gbps"])
            per = cpu / (float(kv["mb"]) / 1024) * 1e3 if float(kv["mb"]) else float("nan")
            cell = f"{thr:.1f} Gbps {per:.0f}ms/GB"
        else:
            name = "hold-" + kv["conns"]; thr = 0.0
            cell = f"{float(kv['opened']):,.0f}/{kv['conns']} {kv.get('peak_rss_mb','?')}MB"
            if float(kv["opened"]) < float(kv["conns"]):
                err = err or "opened<conns"
        cell += at
        if "proxy_unsaturated" in kv:
            cell += f" UNSAT({kv.get('proxy_util', '?')})"
        for flag in ("origin_saturated", "sink_saturated"):
            if flag in kv:
                cell += f" {flag.upper()}"
        pts.setdefault(name, []).append((not err, thr, cell + (f" ERR[{err}]" if err else "")))
    rows = {}
    for name, v in pts.items():
        # clean levels first, then the highest throughput
        rows[name] = max(v, key=lambda t: (t[0], t[1]))[2]
    return rows

order = ["ws-64b", "ws-64kb", "ws-short", "http-1kb", "http-10kb", "http-1mb", "http-10mb", "http-1kb-short", "flood",
         "hold-1000", "hold-5000", "hold-10000", "hold-25000"]
for mode in ["plain", "tls", "h2"]:
    data = [(c.split()[0], load(p)) for c, p in cols if c.split()[1] == mode and os.path.exists(p)]
    if not data:
        continue
    print(f"\n### {mode}\n")
    print("| test | " + " | ".join(c for c, _ in data) + " |")
    print("|---" * (len(data) + 1) + "|")
    for t in order:
        cells = [d.get(t, "—") for _, d in data]
        if any(c != "—" for c in cells):
            print(f"| {t} | " + " | ".join(cells) + " |")
