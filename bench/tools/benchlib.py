"""Parsing of campaign indexes and result files, shared by charts.py and
readme-table.py.

An index (run-campaign.sh) has lines `<proxy> <plain|tls|h2> <results.txt>`;
a results file has one `RESULT median ...` line per test and sweep level. For
each test the best clean level is kept (see sweep_levels in matrix.sh)."""
import datetime, os, re

NAMES = {"myproxy": "rustle", "rustle": "rustle"}


def load(path):
    """{test name: row} for one results file, best valid sweep level."""
    best = {}  # name -> (valid, metric, row)
    for line in open(path, errors="replace"):
        if not line.startswith("RESULT median"):
            continue
        kv = dict(re.findall(r"(\w+)=(\S*)", line))
        m = re.search(r" (?<!gen_)(?<!up_)cpu_s_per_run=([0-9.]+)", line)
        cpu = float(m.group(1)) if m else float("nan")
        valid = not kv.get("err_kinds")
        info = {"conns_level": kv.get("conns"), "unsat": "proxy_unsaturated" in kv,
                "harness_sat": "origin_saturated" in kv or "sink_saturated" in kv,
                "util": kv.get("proxy_util"),
                "tls_negotiated": kv.get("tls_negotiated"),
                "tls_resumed": float(kv["tls_resumed"]) if "tls_resumed" in kv else None}
        if kv["mode"] == "http":
            name = "http-" + kv["url"].rsplit("/", 1)[1] + ("" if kv["keepalive"] == "true" else "-short")
            rps = float(kv["rps"])
            row, metric = {"rps": rps, "us": cpu / (rps * float(kv["elapsed_s"])) * 1e6}, rps
        elif kv["mode"] == "ws":
            size, el = int(kv["size"]), float(kv["elapsed_s"])
            name = "ws-short" if kv["short"] == "true" else "ws-" + (f"{size}b" if size < 1024 else f"{size // 1024}kb")
            rps = float(kv["rps"])
            row = {"rps": rps, "us": cpu / (rps * el) * 1e6 if rps else float("nan"),
                   "p99": float(kv["p99_ms"]), "gbps": float(kv["mbps"]) * 8 / 1000}
            metric = row["gbps"] if size >= 1024 else rps
        elif kv["mode"] == "flood":
            name = "flood"
            row = {"gbps": float(kv["gbps"])}; metric = row["gbps"]
        else:
            name = "hold-" + kv["conns"]
            row = {"opened": float(kv["opened"]), "conns": float(kv["conns"]),
                   "mb": float(kv.get("peak_rss_mb", "nan"))}
            metric = 0.0
            valid = valid and row["opened"] >= row["conns"]
        row.update(info)
        row["valid"] = valid
        key = (valid, metric)
        if name not in best or key > best[name][:2]:
            best[name] = (valid, metric, row)
    return {n: v[2] for n, v in best.items()}


def load_index(idx):
    """{transport: {proxy: {test: row}}}; a proxy whose results are missing is absent."""
    base = os.path.dirname(os.path.dirname(os.path.abspath(idx)))  # bench/
    data = {}
    for line in open(idx):
        if line.startswith("#") or line.startswith("==>"):
            continue
        p = line.split()
        if len(p) >= 3 and os.path.exists(os.path.join(base, p[2])):
            data.setdefault(p[1], {})[NAMES.get(p[0], p[0])] = load(os.path.join(base, p[2]))
    return data


def index_entries(idx):
    """[(proxy, transport, results.txt path)] for every line of an index whose
    results file exists, including runs without any RESULT line (a proxy that
    does not support the test)."""
    base = os.path.dirname(os.path.dirname(os.path.abspath(idx)))  # bench/
    out = []
    for line in open(idx):
        if line.startswith("#") or line.startswith("==>"):
            continue
        p = line.split()
        if len(p) >= 3 and os.path.exists(os.path.join(base, p[2])):
            out.append((NAMES.get(p[0], p[0]), p[1], os.path.join(base, p[2])))
    return out


_MONTHS = {m: i for i, m in enumerate("Jan Feb Mar Apr May Jun Jul Aug Sep Oct Nov Dec".split(), 1)}


def run_header(path):
    """Parameters and date of one run, from the first line of its results.txt
    (`# <proxy> remote auth=on ... Wed Oct 28 14:38:24 CET 2026`) and the
    `seed=` line: {"params": {k: v}, "date": datetime.date | None, "seed": str | None}."""
    with open(path, errors="replace") as f:
        head = f.readline()
        seed = None
        for line in f:
            if line.startswith("seed="):
                seed = line.strip().split("=", 1)[1]
                break
            if line.startswith("RESULT"):
                break
    m = re.search(r"\b([A-Z][a-z]{2}) +(\d{1,2}) \d\d:\d\d:\d\d \S+ (\d{4})\s*$", head)
    date = datetime.date(int(m.group(3)), _MONTHS[m.group(1)], int(m.group(2))) if m else None
    return {"params": dict(re.findall(r"(\w+)=(\S*)", head)), "date": date, "seed": seed}
