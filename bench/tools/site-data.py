#!/usr/bin/env python3
"""bench/data/benchmarks.json: the published benchmark numbers as one
structured file, read by https://github.com/EdouardCourty/rustle-site at the
release tag (see bench/data/README.md for the contract).

    tools/site-data.py                 print the JSON
    tools/site-data.py --write         write bench/data/benchmarks.json
    tools/site-data.py --check         fail when the data files are stale or invalid

Inputs: the campaign indexes (same parsing and same "best clean sweep level"
rule as charts.py and readme-table.py, through benchlib.py) and
bench/data/campaign.json (hand-edited: hardware, network, which indexes make up
the campaign, limits). With no option the indexes are the ones listed in
campaign.json (the committed published runs); a fresh campaign overrides them:

    --main <index> --ws <index> [--flood-from <index>] [--meta <campaign.json>]

--check re-generates from campaign.json and compares with the committed file.
It also checks bench/data/proxies.json (versions against FAIRNESS.md, TLS and
HTTP/2 support against the `# bench-ports:` lines of dut/*/Dockerfile), the
schema_version of every data file and that each chart file exists.
Nothing is ever taken from two campaigns: a run dated more than a day from
campaign.json's date is an error."""
import datetime, json, os, re, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from benchlib import index_entries, load_index, run_header  # noqa: E402

BENCH = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ROOT = os.path.dirname(BENCH)
DATA = os.path.join(BENCH, "data")
SCHEMA_VERSION = 1
ORDER = ["rustle", "3proxy", "squid", "gost", "tinyproxy"]

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


def die(msg):
    sys.exit(f"site-data: {msg}")


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


def build(meta, main_idx, ws_idx, ktls_idx, scaling_idx, flood_idx):
    data_main = load_index(main_idx)
    data_ws = load_index(ws_idx) if ws_idx else {}
    data_ktls = load_index(ktls_idx) if ktls_idx else {}
    data_flood = load_index(flood_idx) if flood_idx else None
    support = ports_support()
    unsupported = meta.get("unsupported", {})

    # one campaign: every run within a day of campaign.json's date
    day = datetime.date.fromisoformat(meta["date"])
    seeds, plain_header = set(), None
    for idx in [main_idx, ws_idx, ktls_idx, flood_idx] + [i for _, i in scaling_idx]:
        if not idx:
            continue
        for proxy, tr, path in index_entries(idx):
            h = run_header(path)
            if h["date"] is None or abs((h["date"] - day).days) > 1:
                die(f"{os.path.relpath(path, BENCH)}: run dated {h['date']}, campaign.json says {day}: "
                    "never mix numbers from two campaigns (update bench/data/campaign.json)")
            if h["seed"]:
                seeds.add(h["seed"])
            if plain_header is None and proxy == "rustle" and tr == "plain" and idx == main_idx:
                plain_header = h
    if plain_header is None:
        die("no rustle plain run in the main index")
    if len(seeds) != 1:
        die(f"the runs use several seeds: {sorted(seeds)}")
    params = plain_header["params"]

    campaign = {
        "name": meta["name"], "date": meta["date"], "commit": meta["commit"],
        "hardware": meta["hardware"], "network": meta["network"], "software": meta["software"],
        "seed": int(seeds.pop()), "repeats": int(params["repeat"]), "run_seconds": meta["run_seconds"],
        "proxy_cores": int(params["workers"]), "auth": params["auth"] == "on",
        "noise_pct": meta["noise_pct"],
        "protocol": "auth on, one proxy core and one worker for every proxy, shuffled test order, proxy "
                    "restarted before each test, warmup run apart, median of the repeats, concurrency sweep "
                    "per test (best clean level kept, at_conns)",
        "fairness": "bench/FAIRNESS.md",
        "results": "bench/RESULTS.md",
    }

    tests = []
    for s in specs():
        tr = s["transport"]
        data = data_ws if tr in ("ws", "ws-tls") else data_main
        flood_note = ""
        if s["id"] == "plain-flood" and data_flood is not None:
            data = data_flood
            flood_note = " Measured on one host over loopback (the three-machine link caps tunnels)."
        values, at_conns, flags, status = {}, {}, {}, {}
        for p in ORDER:
            v, at, fl, st = cell(data, support, unsupported, tr, p, s["test"], s["key"])
            values[p], at_conns[p], flags[p], status[p] = v, at, fl, st
        if all(v is None for v in values.values()):
            continue
        t = {
            "id": s["id"], "transport": tr, "label": s["label"], "unit": s["unit"], "direction": s["direction"],
            "conditions": f"{up(s['what'])}. {up(TRANSPORTS[tr][1])}.{flood_note}",
            "chart": CHARTS.get(s["id"]),
            "note": meta["test_notes"].get(s["id"]),
            "values": values, "at_conns": at_conns, "status": status, "flags": flags,
        }
        tests.append(t)
    known = {t["id"] for t in tests}
    for tid in meta["test_notes"]:
        if tid not in known:
            die(f"campaign.json test_notes: unknown test id {tid!r}")

    scaling_points, seen = [], set()
    for cores, idx in sorted(scaling_idx):
        rows = load_index(idx).get("plain", {})
        values, at_conns, flags = {}, {}, {}
        for p in ORDER:
            r = rows.get(p, {}).get("http-1kb")
            ok = r and r["valid"]
            values[p] = round(r["rps"], 3) if ok else None
            at_conns[p] = int(r["conns_level"]) if ok and r.get("conns_level") else None
            flags[p] = [f for f, on in (("proxy_unsaturated", r["unsat"]), ("harness_saturated", r["harness_sat"])) if on] if ok else []
            if ok:
                seen.add(p)
        scaling_points.append({"cores": cores, "values": values, "at_conns": at_conns, "flags": flags})
    scaling = []
    if scaling_points:
        scaling.append({
            "id": "scaling-plain-1kb", "label": "Plain HTTP, 1 KB responses, 1 to 8 proxy cores",
            "unit": "req/s", "direction": "higher",
            "conditions": "Same hosts, seed and protocol as the single-core matrix, one test (http-1kb), "
                          "connection levels 64, 256, 1,024 and 4,096, best clean level kept. The proxy gets n "
                          "physical cores and n workers.",
            "not_measured": [p for p in ORDER if p not in seen],
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
            ktls.append({"id": s["id"], "label": s["label"], "unit": s["unit"], "direction": s["direction"],
                         "user_space": a[0], "ktls": b[0]})

    return {
        "schema_version": SCHEMA_VERSION,
        "campaign": campaign,
        "proxies": ORDER,
        "tests": tests,
        "scaling": scaling,
        "ktls": ktls,
        "limits": meta["limits"],
    }


def sources(meta, args):
    s = meta["sources"]
    main = abspath(args.get("main") or s["main"])
    ws = args.get("ws") or s.get("ws")
    ktls = s.get("ktls")
    scaling = [(e["cores"], abspath(e["index"])) for e in s.get("scaling", [])]
    return main, abspath(ws) if ws else None, abspath(ktls) if ktls else None, scaling, \
        abspath(args["flood-from"]) if args.get("flood-from") else None


def generate(args):
    meta_path = abspath(args.get("meta") or os.path.join(DATA, "campaign.json"))
    meta = load_json(meta_path)
    if meta.get("schema_version") != SCHEMA_VERSION:
        die(f"{meta_path}: schema_version {meta.get('schema_version')!r}, expected {SCHEMA_VERSION}")
    out = build(meta, *sources(meta, args))
    return json.dumps(out, indent=2, ensure_ascii=False) + "\n"


def check_proxies(problems):
    path = os.path.join(DATA, "proxies.json")
    try:
        doc = load_json(path)
    except (OSError, ValueError) as e:
        problems.append(f"{path}: {e}")
        return
    if doc.get("schema_version") != SCHEMA_VERSION:
        problems.append(f"proxies.json: schema_version {doc.get('schema_version')!r}, expected {SCHEMA_VERSION}")
    ids = [p.get("id") for p in doc.get("proxies", [])]
    if ids != ORDER:
        problems.append(f"proxies.json: proxies are {ids}, expected {ORDER}")
    fairness = open(os.path.join(BENCH, "FAIRNESS.md")).read()
    support = ports_support()
    for p in doc.get("proxies", []):
        pid = p.get("id")
        row = next((l for l in fairness.splitlines() if l.startswith(f"| **{p.get('name')}**")), None)
        if row is None:
            problems.append(f"proxies.json: {pid}: no row in FAIRNESS.md's table")
        elif p.get("version") not in row:
            problems.append(f"proxies.json: {pid}: version {p.get('version')!r} is not the one in FAIRNESS.md")
        s = support.get(pid, {"tls": False, "h2": False})
        for proto in ("tls", "h2"):
            if bool(p.get("protocols", {}).get(proto)) != s[proto]:
                problems.append(f"proxies.json: {pid}: protocols.{proto} disagrees with dut/{pid}/Dockerfile bench-ports")


def check(args):
    problems = []
    try:
        fresh = generate(args)
    except SystemExit as e:
        problems.append(str(e).replace("site-data: ", "", 1))
        fresh = None
    committed_path = os.path.join(DATA, "benchmarks.json")
    if fresh is not None:
        try:
            committed = open(committed_path).read()
        except OSError:
            committed = None
        if committed != fresh:
            problems.append("bench/data/benchmarks.json is stale: run `python3 bench/tools/site-data.py --write`")
        else:
            for t in json.loads(fresh)["tests"]:
                if t["chart"] and not os.path.isfile(os.path.join(ROOT, "docs", "benchmarks", t["chart"])):
                    problems.append(f"test {t['id']}: chart docs/benchmarks/{t['chart']} does not exist")
    check_proxies(problems)
    for name in ("benchmarks.json", "campaign.json", "proxies.json"):
        try:
            v = load_json(os.path.join(DATA, name)).get("schema_version")
        except (OSError, ValueError):
            continue
        if v != SCHEMA_VERSION:
            problems.append(f"bench/data/{name}: schema_version {v!r}, expected {SCHEMA_VERSION}")
    cfg = os.path.join(ROOT, "docs", "config.json")
    try:
        v = load_json(cfg).get("schema_version")
        if v != SCHEMA_VERSION:
            problems.append(f"docs/config.json: schema_version {v!r}, expected {SCHEMA_VERSION}")
    except (OSError, ValueError) as e:
        problems.append(f"docs/config.json: {e}")
    if problems:
        print("\n".join(f"site-data: {p}" for p in problems), file=sys.stderr)
        sys.exit(1)
    print("site data up to date")


def main(argv):
    args, flags, i = {}, set(), 0
    while i < len(argv):
        a = argv[i]
        if a in ("--write", "--check"):
            flags.add(a)
        elif a in ("--main", "--ws", "--flood-from", "--meta") and i + 1 < len(argv):
            args[a[2:]] = argv[i + 1]
            i += 1
        else:
            sys.exit(__doc__)
        i += 1
    if "--check" in flags:
        if args:
            sys.exit("--check takes no source option: it checks the committed files against campaign.json")
        return check(args)
    out = generate(args)
    if "--write" in flags:
        path = os.path.join(DATA, "benchmarks.json")
        with open(path, "w") as f:
            f.write(out)
        print(f"data written to {os.path.relpath(path, ROOT)}")
    else:
        sys.stdout.write(out)


if __name__ == "__main__":
    main(sys.argv[1:])
