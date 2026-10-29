#!/usr/bin/env python3
"""Aggregation: the campaign manifest (bench/data/<id>/campaign.json `sources`) and the raw
`run.json` files it lists become bench/data/<id>/benchmarks.json.

    tools/aggregate.py                      print the JSON of the latest campaign
    tools/aggregate.py --write              write bench/data/<latest>/benchmarks.json
    tools/aggregate.py --campaign <id>      another campaign than index.json's `latest`
    tools/aggregate.py --sources <file>     use the runs of a fragment (written by run-campaign.sh /
                                            run-scaling.sh) instead of those of campaign.json for
                                            the roles it holds; repeatable
    tools/aggregate.py --report             list the points that are not clean (invalid, unsaturated,
                                            missing, TLS footing that differs); exit 1 if any

Rules applied to the raw runs (they were the rules of the text tools before): for each test the
best clean sweep level of a proxy (a point with a client error, or with fewer tunnels than asked,
is never kept while a clean one exists); a proxy that cannot do a test is `unsupported`, one that
was not run `not_measured`, never a zero. Nothing is ever taken from two campaigns: a run dated
more than a day from campaign.json's date is an error, and what campaign.json states (seed,
repeats, proxy cores, auth) must be what the runs say."""
import datetime, json, os, re, sys

BENCH = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ROOT = os.path.dirname(BENCH)
DATA = os.path.join(BENCH, "data")
SCHEMA_VERSION = 1
NAN = float("nan")

# transport -> (key in the index, how the proxy is reached)
TRANSPORTS = {
    "plain": ("plain", "plain HTTP proxy port"),
    "tls": ("tls", "HTTPS proxy (TLS 1.3, AES-128-GCM, session resumption on)"),
    "tls-full-handshake": ("tlsfull", "HTTPS proxy (TLS 1.3, AES-128-GCM), a full handshake on every connection"),
    "h2": ("h2", "HTTP/2 to the proxy over TLS (ALPN h2), one HTTP/2 connection per client connection, sequential requests"),
    "ws": ("plain", "WebSocket upgrade on the plain HTTP proxy port"),
    "ws-tls": ("tls", "WebSocket upgrade through the HTTPS proxy (TLS 1.3, AES-128-GCM)"),
}
# needs a TLS / HTTP/2 listener on the proxy (dut/<proxy>/Dockerfile bench-ports)
NEEDS = {"tls": "tls", "tls-full-handshake": "tls", "h2": "h2", "ws-tls": "tls"}

HTTP_TESTS = [  # (id suffix, bench test, label, description)
    ("1kb", "http-1kb", "HTTP, 1 KB responses", "1 KB responses, keep-alive"),
    ("10kb", "http-10kb", "HTTP, 10 KB responses", "10 KB responses, keep-alive"),
    ("short", "http-1kb-short", "HTTP, 1 KB responses, a new connection per request", "1 KB responses, no keep-alive"),
    ("1mb", "http-1mb", "HTTP, 1 MB responses", "1 MB responses, keep-alive"),
    ("10mb", "http-10mb", "HTTP, 10 MB responses", "10 MB responses, keep-alive"),
]
FLOOD = ("flood", "flood", "CONNECT tunnels, bulk transfer", "bulk upload through CONNECT tunnels")
HOLDS = [1000, 5000, 10000, 25000]
CHARTS = {  # test id -> chart file in docs/benchmarks/
    "plain-1kb": "plain-1kb.svg", "plain-1mb": "plain-1mb.svg", "plain-cpu": "plain-cpu.svg",
    "plain-flood": "plain-flood.svg", "plain-ram-25000": "plain-ram.svg",
    "tls-1kb": "tls-1kb.svg", "tls-short": "tls-short.svg", "tls-short-full": "tls-short-full.svg",
    "h2-1kb": "h2-1kb.svg",
    "ws-64b-rps": "ws-64b-rps.svg", "ws-64b-latency": "ws-64b-latency.svg", "ws-cpu": "ws-cpu.svg",
    "ws-64kb-gbps": "ws-64kb-gbps.svg",
}


def specs():
    """Every published test, in reading order: dicts with the id, the transport,
    the bench test and the key to read from its result row."""
    out = []

    def add(tid, tr, test, key, label, unit, direction, what):
        out.append(dict(id=tid, transport=tr, test=test, key=key, label=label, unit=unit,
                        direction=direction, what=what))

    for tr in ("plain", "tls", "h2"):
        for suffix, test, label, what in HTTP_TESTS:
            add(f"{tr}-{suffix}", tr, test, "rps", label, "req/s", "higher", what)
        if tr == "tls":
            add("tls-short-full", "tls-full-handshake", "http-1kb-short", "rps",
                "HTTP, 1 KB responses, a new connection per request, full handshake", "req/s", "higher",
                "1 KB responses, no keep-alive")
        add(f"{tr}-flood", tr, FLOOD[1], "gbps", FLOOD[2], "Gbit/s", "higher", FLOOD[3])
        if tr == "plain":
            add("plain-cpu", "plain", "http-1kb", "us", "CPU cost per request (1 KB)", "µs/req", "lower",
                "proxy CPU per request, 1 KB responses, keep-alive")
        for n in HOLDS:
            add(f"{tr}-ram-{n}", tr, f"hold-{n}", "mb", f"Memory with {n:,} open tunnels", "MB", "lower",
                f"{n:,} idle CONNECT tunnels held open, peak RSS of the proxy")
    for tr in ("ws", "ws-tls"):
        add(f"{tr}-64b-rps", tr, "ws-64b", "rps", "WebSocket, 64-byte messages", "msg/s", "higher",
            "64-byte messages, 64 connections, ping-pong")
        add(f"{tr}-64b-latency", tr, "ws-64b", "p99", "WebSocket round trip, 64-byte messages (p99)", "ms", "lower",
            "64-byte messages, 64 connections, ping-pong, p99 latency")
        if tr == "ws":
            add("ws-cpu", "ws", "ws-64b", "us", "CPU cost per WebSocket message", "µs/msg", "lower",
                "proxy CPU per 64-byte message")
        add(f"{tr}-64kb-gbps", tr, "ws-64kb", "gbps", "WebSocket, 64 KB messages", "Gbit/s", "higher",
            "64 KB frames, 8 connections, 8 frames in flight")
        add(f"{tr}-short", tr, "ws-short", "rps", "WebSocket, one connection per exchange", "msg/s", "higher",
            "one connection per exchange")
    return out


def up(text):
    """First letter in capitals, the rest untouched (str.capitalize lowercases "HTTP")."""
    return text[:1].upper() + text[1:]


def extras_of(s):
    """Extra by-proxy maps a test carries next to its values: {name in benchmarks.json: key of the row}."""
    out = {}
    if s["key"] in ("rps", "gbps"):
        if s["test"].startswith("http-") or s["test"] in ("ws-64b", "ws-short"):
            out["cpu_us"] = "us"
        if s["test"] in ("flood", "ws-64kb"):
            out["cpu_ms_per_gb"] = "ms_per_gb"
        if s["test"].startswith("ws-") and s["test"] != "ws-64kb":
            out["p99_ms"] = "p99"
        out["proxy_util"] = "util"
    return out


def die(msg):
    sys.exit(f"aggregate: {msg}")


def load_json(path):
    with open(path) as f:
        return json.load(f)


def abspath(p):
    return p if os.path.isabs(p) else os.path.join(BENCH, p)


def ports_support():
    """{proxy: {"tls": bool, "h2": bool}} from `# bench-ports:` in dut/*/Dockerfile."""
    out = {}
    dut = os.path.join(BENCH, "dut")
    for p in sorted(os.listdir(dut)):
        df = os.path.join(dut, p, "Dockerfile")
        if not os.path.isfile(df):
            continue
        text = open(df).read()
        m = re.search(r"^# bench-ports:(.*)$", text, re.M)
        words = m.group(1) if m else ""
        out[p] = {"tls": "tls=" in words, "h2": "h2=" in words}
    return out


def cell(data, support, unsupported, transport, proxy, test, key):
    """(value, at_conns, flags, status) of one proxy on one test. `unsupported`
    ({transport: [proxy]}, campaign.json) lists the proxies that cannot do the
    test at all: their runs are listed as unsupported, never as zero."""
    ix, _ = TRANSPORTS[transport]
    rows = data.get(ix, {}).get(proxy)
    if proxy in unsupported.get(transport, []):
        if rows and rows.get(test, {}).get("valid"):
            die(f"campaign.json says {proxy} does not support {transport}, but its {test} run is clean")
        return None, None, [], "unsupported"
    if rows is None:
        need = NEEDS.get(transport)
        if need and not support.get(proxy, {}).get(need, False):
            return None, None, [], "unsupported"
        return None, None, [], "not_measured"
    if not rows:
        return None, None, [], "unsupported"
    r = rows.get(test)
    if r is None:
        return None, None, [], "not_measured"
    if not r["valid"] or key not in r or r[key] != r[key]:
        return None, None, [], "invalid"
    flags = []
    if r["unsat"]:
        flags.append("proxy_unsaturated")
    if r["harness_sat"]:
        flags.append("harness_saturated")
    v = round(r[key], 1 if key == "mb" else 3)
    at = None if test.startswith("hold-") or not r.get("conns_level") else int(r["conns_level"])
    return v, at, flags, "ok"




# ---- the raw runs -----------------------------------------------------------

def num(v):
    return NAN if v is None else v


def row_of(e):
    """The figures one result entry contributes to the tables: (metric to rank sweep levels by, row)."""
    mode, wl = e["mode"], e["workload"]
    m = {k: num(v) for k, v in e["measured"].items()}
    cpu = e["proxy"]["cpu_s_per_run"]
    sat = e.get("saturation") or {}
    flags = sat.get("flags", [])
    tls = e["generator"].get("tls") or {}
    info = {"origin": sat.get("origin_util"), "conns_level": wl.get("conns"), "unsat": "proxy_unsaturated" in flags,
            "harness_sat": "origin_saturated" in flags or "sink_saturated" in flags, "util": sat.get("proxy_util"),
            "tls_negotiated": tls.get("negotiated"), "tls_resumed": tls.get("resumed"), "valid": e["valid"]}
    if mode == "http":
        rps = m["rps"]
        row, metric = {"rps": rps, "us": cpu / (rps * m["elapsed_s"]) * 1e6 if rps else NAN}, rps
    elif mode == "ws":
        rps, size = m["rps"], wl["size"]
        gb = m["mbps"] * m["elapsed_s"] / 1000  # mbps is MB/s: GB = 10^9 bytes, like the Gbit/s
        row = {"rps": rps, "us": cpu / (rps * m["elapsed_s"]) * 1e6 if rps else NAN, "p99": m["p99_ms"],
               "gbps": m["mbps"] * 8 / 1000, "ms_per_gb": cpu / gb * 1e3 if gb else NAN}
        metric = row["gbps"] if size >= 1024 else rps
    elif mode == "flood":
        row = {"gbps": m["gbps"], "ms_per_gb": cpu / (m["mb"] / 1000) * 1e3 if m["mb"] else NAN}
        metric = m["gbps"]
    else:
        row = {"opened": m["opened"], "conns": float(wl["conns"]), "mb": e["proxy"]["peak_rss_mb"]}
        metric = 0.0
    row.update(info)
    return metric, row


def best_rows(results):
    """{test: row}: for each test the best valid sweep level (valid first, then the metric; the first
    of equals wins)."""
    best = {}
    for e in results:
        metric, row = row_of(e)
        key = (row["valid"], metric)
        if e["test"] not in best or key > best[e["test"]][0]:
            best[e["test"]] = (key, row)
    return {t: v[1] for t, v in best.items()}


def abspath(p):
    return p if os.path.isabs(p) else os.path.join(BENCH, p)


def load_runs(runs):
    """Read the runs of a manifest. Returns ({role: {transport: {proxy: {test: row}}}}, {cores: that}
    for the scaling role, [(role, run document)]); a proxy listed with no result is `{}`."""
    roles, scaling, docs = {}, {}, []
    for r in runs:
        path = os.path.join(abspath(r["run"]), "run.json")
        if not os.path.isfile(path):
            continue  # a run that never started: the proxy is simply absent
        doc = load_json(path)
        if doc.get("schema_version") != SCHEMA_VERSION:
            die(f"{path}: schema_version {doc.get('schema_version')!r}")
        if (doc["proxy"], doc["transport"]) != (r["proxy"], r["transport"]):
            die(f"{path} is {doc['proxy']}/{doc['transport']}, the manifest says {r['proxy']}/{r['transport']}")
        if (doc["matrix"] == "ws") != (r["role"] == "ws"):
            die(f"{path}: a {doc['matrix']} run in the {r['role']} role")
        docs.append((r["role"], doc))
        target = scaling.setdefault(r["cores"], {}) if r["role"] == "scaling" else roles.setdefault(r["role"], {})
        target.setdefault(r["transport"], {})[r["proxy"]] = best_rows(doc["results"])
    return roles, scaling, docs


# ---- the campaign -----------------------------------------------------------

def index_file():
    return os.path.join(DATA, "index.json")


def latest_id():
    return load_json(index_file())["latest"]


def campaign_dir(cid):
    return os.path.join(DATA, cid)


def proxy_order(cid):
    """The proxy ids of a campaign, in column order (its proxies.json)."""
    return [p["id"] for p in load_json(os.path.join(campaign_dir(cid), "proxies.json"))["proxies"]]


def check_one_campaign(meta, docs):
    """Every run within a day of campaign.json's date, one seed, and what campaign.json states is what the runs say."""
    day = datetime.date.fromisoformat(meta["date"])
    seeds = set()
    for _, d in docs:
        started = datetime.date.fromisoformat(d["started"][:10])
        if abs((started - day).days) > 1:
            die(f"a run of {d['proxy']}/{d['transport']} started on {started}, campaign.json says {day}: "
                "never mix numbers from two campaigns (update the campaign's folder under bench/data/)")
        seeds.add(d["seed"])
    if len(seeds) != 1:
        die(f"the runs use several seeds: {sorted(seeds, key=str)}")
    first = next((d for role, d in docs if role == "main" and d["proxy"] == "rustle" and d["transport"] == "plain"), None)
    if first is None:
        die("no rustle plain run among the main runs")
    for field, found in (("seed", seeds.pop()), ("repeats", first["repeat"]), ("proxy_cores", first["workers"]),
                         ("auth", first["auth"])):
        if meta[field] != found:
            die(f"campaign.json says {field} = {meta[field]!r}, the runs say {found!r}")


def build(meta, order, runs):
    roles, scaling_data, docs = load_runs(runs)
    check_one_campaign(meta, docs)
    data_main, data_ws = roles.get("main", {}), roles.get("ws", {})
    data_ktls, data_flood = roles.get("ktls", {}), roles.get("flood")
    support = ports_support()
    unsupported = meta.get("unsupported", {})

    tests = []
    for s in specs():
        tr = s["transport"]
        data = data_ws if tr in ("ws", "ws-tls") else data_main
        flood_note = ""
        if s["id"] == "plain-flood" and data_flood is not None:
            data = data_flood
            flood_note = " Measured on one host over loopback (the three-machine link caps tunnels)."
        values, at_conns, flags, status = {}, {}, {}, {}
        for p in order:
            v, at, fl, st = cell(data, support, unsupported, tr, p, s["test"], s["key"])
            values[p], at_conns[p], flags[p], status[p] = v, at, fl, st
        if all(v is None for v in values.values()):
            continue
        t = {
            "id": s["id"], "transport": tr, "label": s["label"], "unit": s["unit"], "direction": s["direction"],
            "conditions": f"{up(s['what'])}. {up(TRANSPORTS[tr][1])}.{flood_note}",
            "chart": CHARTS.get(s["id"]),
            "values": values, "at_conns": at_conns, "status": status, "flags": flags,
        }
        for name, key in extras_of(s).items():
            t[name] = {p: cell(data, support, unsupported, tr, p, s["test"], key)[0] for p in order}
        tests.append(t)
    known = {t["id"] for t in tests}
    for tid in meta["test_notes"]:
        if tid not in known:
            die(f"campaign.json test_notes: unknown test id {tid!r}")

    scaling_points, seen = [], set()
    for cores in sorted(scaling_data):
        rows = scaling_data[cores].get("plain", {})
        values, at_conns, flags, util, origin = {}, {}, {}, {}, {}
        for p in order:
            r = rows.get(p, {}).get("http-1kb")
            ok = r and r["valid"]
            values[p] = round(r["rps"], 3) if ok else None
            at_conns[p] = int(r["conns_level"]) if ok and r.get("conns_level") else None
            flags[p] = [f for f, on in (("proxy_unsaturated", r["unsat"]), ("harness_saturated", r["harness_sat"])) if on] if ok else []
            util[p] = r["util"] if ok else None
            origin[p] = r["origin"] if ok else None
            if ok:
                seen.add(p)
        scaling_points.append({"cores": cores, "values": values, "at_conns": at_conns, "flags": flags,
                               "proxy_util": util, "origin_util": origin})
    scaling = []
    if scaling_points:
        scaling.append({
            "id": "scaling-plain-1kb", "label": "Plain HTTP, 1 KB responses, 1 to 8 proxy cores",
            "unit": "req/s", "direction": "higher",
            "conditions": "Same hosts, seed and protocol as the single-core matrix, one test (http-1kb), "
                          "connection levels 64, 256, 1,024 and 4,096, best clean level kept. The proxy gets n "
                          "physical cores and n workers.",
            "not_measured": [p for p in order if p not in seen],
            "points": scaling_points,
        })

    ktls = []
    if data_ktls:
        for s in specs():
            if s["transport"] != "tls":
                continue
            a = cell(data_main, support, unsupported, "tls", "rustle", s["test"], s["key"])
            b = cell(data_ktls, support, unsupported, "tls", "rustle", s["test"], s["key"])
            if a[0] is None or b[0] is None:
                continue
            entry = {"id": s["id"], "label": s["label"], "unit": s["unit"], "direction": s["direction"],
                     "user_space": a[0], "ktls": b[0]}
            if s["test"] == "flood":
                entry["user_space_cpu_ms_per_gb"] = cell(data_main, support, unsupported, "tls", "rustle", s["test"], "ms_per_gb")[0]
                entry["ktls_cpu_ms_per_gb"] = cell(data_ktls, support, unsupported, "tls", "rustle", s["test"], "ms_per_gb")[0]
            ktls.append(entry)

    return {
        "$schema": "../schema/benchmarks.schema.json",
        "schema_version": SCHEMA_VERSION,
        "campaign": meta["id"],
        "tests": tests,
        "scaling": scaling,
        "ktls": ktls,
    }


def merged_runs(meta, fragments):
    """The manifest's runs, with the roles held by the given fragments replaced by the fragments' runs."""
    runs = meta["sources"]["runs"]
    if not fragments:
        return runs
    roles = {r["role"] for r in fragments}
    return [r for r in runs if r["role"] not in roles] + fragments


def report(runs):
    """Problems that stop a campaign from being published: a point with a client error or missing tunnels, a
    proxy under 90 % of its core (the sweep is too small, the harness or a latency-bound proxy set the number), a
    saturated origin or sink, a proxy without any result, proxies that did not negotiate the same TLS footing."""
    problems, tls = [], {}
    for r in runs:
        path = os.path.join(abspath(r["run"]), "run.json")
        tag = f"[{r['role']} {r['transport']}] {r['proxy']}"
        if not os.path.isfile(path):
            problems.append(f"{tag}: no run.json (the run did not start)")
            continue
        rows = best_rows(load_json(path)["results"])
        if not rows:
            problems.append(f"{tag}: no results (unsupported or failed): list it as unsupported, never as 0")
        for test, row in sorted(rows.items()):
            at = f" @{row['conns_level']}" if row["conns_level"] and not test.startswith("hold-") else ""
            if not row["valid"]:
                problems.append(f"{tag} {test}: INVALID (client errors or tunnels missing)")
            elif row["util"] is None and not test.startswith("hold-"):
                problems.append(f"{tag} {test}: no saturation data")
            elif row["unsat"]:
                problems.append(f"{tag} {test}{at}: proxy only at {row['util']} of its cores: extend the sweep before publishing")
            elif row["harness_sat"]:
                problems.append(f"{tag} {test}{at}: origin or sink saturated: add harness threads")
            if row["tls_negotiated"]:
                tls.setdefault((r["transport"], r["role"]), {}).setdefault(row["tls_negotiated"], set()).add(r["proxy"])
            if row["tls_resumed"] is not None and r["transport"] == "tls" and test.endswith("-short") and row["tls_resumed"] < 0.5:
                problems.append(f"{tag} {test}: only {row['tls_resumed']:.0%} of the connections were resumed (does it issue session tickets?)")
            if row["tls_resumed"] is not None and r["transport"] == "tlsfull" and row["tls_resumed"] > 0.05:
                problems.append(f"{tag} {test}: {row['tls_resumed']:.0%} resumed, expected none")
    for (transport, role), seen in tls.items():
        if len(seen) > 1:
            detail = "; ".join(f"{k}: {', '.join(sorted(v))}" for k, v in sorted(seen.items()))
            problems.append(f"[{role} {transport}] the proxies did not negotiate the same TLS version/cipher ({detail}): align them or say so next to the numbers")
    return problems


def generate(cid, sources=None):
    """benchmarks.json of a campaign, as text."""
    meta_path = os.path.join(campaign_dir(cid), "campaign.json")
    if not os.path.isfile(meta_path):
        die(f"{meta_path} does not exist: a new campaign starts with its folder, campaign.json and proxies.json "
            "(see bench/data/README.md)")
    meta = load_json(meta_path)
    if meta.get("schema_version") != SCHEMA_VERSION:
        die(f"{meta_path}: schema_version {meta.get('schema_version')!r}, expected {SCHEMA_VERSION}")
    if meta.get("id") != cid:
        die(f"{meta_path}: id {meta.get('id')!r} is not the campaign folder {cid!r}")
    return json.dumps(build(meta, proxy_order(cid), merged_runs(meta, sources)), indent=2, ensure_ascii=False) + "\n"


def report_of(cid, sources=None):
    return report(merged_runs(load_json(os.path.join(campaign_dir(cid), "campaign.json")), sources))


def main(argv):
    cid, write, sources, do_report, i = None, False, None, False, 0
    while i < len(argv):
        a = argv[i]
        if a == "--write":
            write = True
        elif a == "--report":
            do_report = True
        elif a == "--campaign" and i + 1 < len(argv):
            cid, i = argv[i + 1], i + 1
        elif a == "--sources" and i + 1 < len(argv):
            sources = (sources or []) + load_json(argv[i + 1])["runs"]
            i += 1
        else:
            sys.exit(__doc__)
        i += 1
    cid = cid or latest_id()
    if do_report:
        problems = report_of(cid, sources)
        print("\n".join(problems) if problems else "All best levels are clean and saturated.")
        if problems:
            print(f"\n{len(problems)} point(s) need attention before publishing.")
            sys.exit(1)
        return
    out = generate(cid, sources)
    if write:
        path = os.path.join(campaign_dir(cid), "benchmarks.json")
        with open(path, "w") as f:
            f.write(out)
        print(f"data written to {os.path.relpath(path, ROOT)}")
    else:
        sys.stdout.write(out)


if __name__ == "__main__":
    main(sys.argv[1:])
