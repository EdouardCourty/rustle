#!/usr/bin/env python3
"""bench/data/<campaign>/benchmarks.json: the published benchmark numbers of one
campaign as a structured file, read by https://github.com/EdouardCourty/rustle-site
at the release tag (see bench/data/README.md for the contract).

    tools/site-data.py                 print the JSON of the latest campaign
    tools/site-data.py --write         write bench/data/<latest>/benchmarks.json
    tools/site-data.py --check         fail when any data file is stale or invalid

    --campaign <id>                    another campaign than index.json's `latest`

Inputs: the campaign indexes (same parsing and same "best clean sweep level"
rule as charts.py and readme-table.py, through benchlib.py) and the campaign's
hand-edited campaign.json (hardware, network, which indexes make up the
campaign, limits, notes). The seed, repeats, proxy cores and auth it states are
checked against the runs. With no source option the indexes are the ones listed
in campaign.json (the committed published runs); a fresh campaign overrides them:

    --main <index> --ws <index> [--flood-from <index>] [--meta <campaign.json>]

--check, for every campaign of index.json, what a JSON Schema cannot say (the
files themselves are validated by `make schema-check`, check-jsonschema): ids
agree between the folder, index.json and the files, the by-proxy maps hold
exactly the campaign's proxies, `$schema` pointers are exact, project URLs are
https, benchmarks.json is what the indexes generate. For the latest campaign it
also checks proxies.json against FAIRNESS.md (versions) and dut/*/Dockerfile
(TLS and HTTP/2 support), that each chart file exists and that the README
benchmark table is the one benchmarks.json gives.
Nothing is ever taken from two campaigns: a run dated more than a day from
campaign.json's date is an error."""
import datetime, json, os, re, sys
from urllib.parse import urlsplit

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from benchlib import index_entries, load_index, run_header  # noqa: E402

BENCH = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ROOT = os.path.dirname(BENCH)
DATA = os.path.join(BENCH, "data")
SCHEMA_VERSION = 1

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


def build(meta, order, main_idx, ws_idx, ktls_idx, scaling_idx, flood_idx):
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

    # what campaign.json states must be what the runs say
    for field, found in (("seed", int(seeds.pop())), ("repeats", int(params["repeat"])),
                         ("proxy_cores", int(params["workers"])), ("auth", params["auth"] == "on")):
        if meta[field] != found:
            die(f"campaign.json says {field} = {meta[field]!r}, the runs say {found!r}")

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
        tests.append(t)
    known = {t["id"] for t in tests}
    for tid in meta["test_notes"]:
        if tid not in known:
            die(f"campaign.json test_notes: unknown test id {tid!r}")

    scaling_points, seen = [], set()
    for cores, idx in sorted(scaling_idx):
        rows = load_index(idx).get("plain", {})
        values, at_conns, flags = {}, {}, {}
        for p in order:
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
            ktls.append({"id": s["id"], "label": s["label"], "unit": s["unit"], "direction": s["direction"],
                         "user_space": a[0], "ktls": b[0]})

    return {
        "$schema": "../schema/benchmarks.schema.json",
        "schema_version": SCHEMA_VERSION,
        "campaign": meta["id"],
        "tests": tests,
        "scaling": scaling,
        "ktls": ktls,
    }


def index_file():
    return os.path.join(DATA, "index.json")


def latest_id():
    return load_json(index_file())["latest"]


def campaign_dir(cid):
    return os.path.join(DATA, cid)


def proxy_order(cid):
    """The proxy ids of a campaign, in column order (its proxies.json)."""
    return [p["id"] for p in load_json(os.path.join(campaign_dir(cid), "proxies.json"))["proxies"]]


def sources(meta, args):
    s = meta["sources"]
    main = abspath(args.get("main") or s["main"])
    ws = args.get("ws") or s.get("ws")
    ktls = s.get("ktls")
    scaling = [(e["cores"], abspath(e["index"])) for e in s.get("scaling", [])]
    return main, abspath(ws) if ws else None, abspath(ktls) if ktls else None, scaling, \
        abspath(args["flood-from"]) if args.get("flood-from") else None


def generate(args, cid):
    meta_path = abspath(args.get("meta") or os.path.join(campaign_dir(cid), "campaign.json"))
    if not os.path.isfile(meta_path):
        die(f"{meta_path} does not exist: a new campaign starts with its folder, campaign.json and proxies.json "
            "(see bench/data/README.md)")
    meta = load_json(meta_path)
    if meta.get("schema_version") != SCHEMA_VERSION:
        die(f"{meta_path}: schema_version {meta.get('schema_version')!r}, expected {SCHEMA_VERSION}")
    if meta.get("id") != cid:
        die(f"{meta_path}: id {meta.get('id')!r} is not the campaign folder {cid!r}")
    out = build(meta, proxy_order(cid), *sources(meta, args))
    return json.dumps(out, indent=2, ensure_ascii=False) + "\n"


def check_file(problems, path, pointer):
    """The file is valid JSON and points at its schema with the exact relative path. Returns the document."""
    rel = os.path.relpath(path, ROOT)
    try:
        doc = load_json(path)
    except (OSError, ValueError) as e:
        problems.append(f"{rel}: {e}")
        return None
    if doc.get("$schema") != pointer or not os.path.isfile(os.path.normpath(os.path.join(os.path.dirname(path), pointer))):
        problems.append(f"{rel}: $schema must be {pointer!r} and point at an existing schema")
    return doc


def check_proxies_latest(problems, doc):
    """Versions against FAIRNESS.md and protocols against the Dockerfiles: they describe the
    current setup, so only the latest campaign can be compared with them."""
    fairness = open(os.path.join(BENCH, "FAIRNESS.md")).read()
    support = ports_support()
    for p in doc["proxies"]:
        pid = p["id"]
        row = next((l for l in fairness.splitlines() if l.startswith(f"| **{p['name']}**")), None)
        if row is None:
            problems.append(f"proxies.json: {pid}: no row in FAIRNESS.md's table")
        elif p["version"] not in row:
            problems.append(f"proxies.json: {pid}: version {p['version']!r} is not the one in FAIRNESS.md")
        s = support.get(pid, {"tls": False, "h2": False})
        for proto in ("tls", "h2"):
            if bool(p["protocols"].get(proto)) != s[proto]:
                problems.append(f"proxies.json: {pid}: protocols.{proto} disagrees with dut/{pid}/Dockerfile bench-ports")


def fmt_ram(mb):
    return f"{mb / 1024:.1f} GB" if mb >= 1024 else f"{mb:.0f} MB"


# README table rows: (label, test id, formatter)
README_ROWS = [
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
    """The summary table of README.md as benchmarks.json gives it (same layout and bolding as readme-table.py)."""
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


def check_readme_table(problems, bench, proxies):
    text = open(os.path.join(ROOT, "README.md")).read()
    a, b = "<!-- bench:table:start -->", "<!-- bench:table:end -->"
    if a not in text or b not in text:
        problems.append("README.md: bench:table markers not found")
        return
    current = text[text.index(a) + len(a):text.index(b)].strip()
    if current != readme_table(bench, proxies):
        problems.append("README.md: the benchmark table is not the one bench/data/<latest>/benchmarks.json gives: "
                        "run `make bench-publish` (README table) or `tools/readme-table.py`, or `make data`")


def check_urls(problems, proxies):
    for p in proxies["proxies"]:
        for field in ("repository", "website"):
            if field in p:
                u = urlsplit(p[field])
                if u.scheme != "https" or not u.netloc or any(c.isspace() for c in p[field]):
                    problems.append(f"proxies.json: {p['id']}.{field} {p[field]!r} is not a well-formed https URL")


def check_campaign(problems, entry, latest):
    cid, cdir = entry["id"], os.path.join(DATA, entry["path"])
    if os.path.basename(cdir.rstrip("/")) != cid:
        problems.append(f"index.json: campaign {cid}: path {entry['path']!r} is not its folder")
    if not os.path.isdir(cdir):
        problems.append(f"index.json: campaign {cid}: {entry['path']} does not exist")
        return
    camp = check_file(problems, os.path.join(cdir, "campaign.json"), "../schema/campaign.schema.json")
    prox = check_file(problems, os.path.join(cdir, "proxies.json"), "../schema/proxies.schema.json")
    bench = check_file(problems, os.path.join(cdir, "benchmarks.json"), "../schema/benchmarks.schema.json")
    for name, doc, key in (("campaign.json", camp, "id"), ("proxies.json", prox, "campaign"), ("benchmarks.json", bench, "campaign")):
        if doc is not None and doc.get(key) != cid:
            problems.append(f"{cid}/{name}: {key} is {doc.get(key)!r}, expected {cid!r}")
    if camp is not None:
        for field in ("date", "rustle_version"):
            if camp.get(field) != entry[field]:
                problems.append(f"index.json: campaign {cid}: {field} {entry[field]!r} differs from campaign.json's {camp.get(field)!r}")
    ids = [p["id"] for p in prox["proxies"]] if prox is not None else None
    if prox is not None:
        if ids != entry["proxies"]:
            problems.append(f"index.json: campaign {cid}: proxies {entry['proxies']} differ from proxies.json's {ids}")
        check_urls(problems, prox)
        if latest:
            check_proxies_latest(problems, prox)
    if bench is not None and ids is not None:
        # every by-proxy map has exactly the campaign's proxies
        for t in bench["tests"] + [pt for sc in bench["scaling"] for pt in sc["points"]]:
            for field in ("values", "at_conns", "flags") + (("status",) if "status" in t else ()):
                if sorted(t[field]) != sorted(ids):
                    problems.append(f"{cid}/benchmarks.json: {t.get('id', t.get('cores'))}: {field} keys are not the campaign's proxies")
        if latest:
            if prox is not None:
                check_readme_table(problems, bench, prox)
            for t in bench["tests"]:
                if t["chart"] and not os.path.isfile(os.path.join(ROOT, "docs", "benchmarks", t["chart"])):
                    problems.append(f"{cid}/benchmarks.json: test {t['id']}: chart docs/benchmarks/{t['chart']} does not exist")
    if camp is not None and prox is not None:
        try:
            fresh = generate({}, cid)
        except SystemExit as e:
            problems.append(f"{cid}: " + str(e).replace("site-data: ", "", 1))
            return
        committed = open(os.path.join(cdir, "benchmarks.json")).read() if os.path.isfile(os.path.join(cdir, "benchmarks.json")) else None
        if committed != fresh:
            problems.append(f"{cid}/benchmarks.json is stale: run `python3 bench/tools/site-data.py --write --campaign {cid}`")


def check(args):
    problems = []
    try:
        check_all(problems)
    except (KeyError, TypeError, IndexError) as e:
        problems.append(f"unexpected structure ({e!r}): run `make schema-check` first, it reports the exact file and path")
    if problems:
        print("\n".join(f"site-data: {p}" for p in problems), file=sys.stderr)
        sys.exit(1)
    print("site data up to date")


def check_all(problems):
    index = check_file(problems, index_file(), "./schema/index.schema.json")
    if index is not None:
        ids = [c["id"] for c in index["campaigns"]]
        if len(set(ids)) != len(ids):
            problems.append("index.json: duplicate campaign ids")
        if index["latest"] not in ids:
            problems.append(f"index.json: latest {index['latest']!r} is not a listed campaign")
        for entry in index["campaigns"]:
            check_campaign(problems, entry, entry["id"] == index["latest"])
        on_disk = sorted(d for d in os.listdir(DATA) if os.path.isdir(os.path.join(DATA, d)) and d != "schema")
        if on_disk != sorted(ids):
            problems.append(f"bench/data: folders {on_disk} differ from the campaigns of index.json {sorted(ids)}")
    cfg = os.path.join(ROOT, "docs", "config.json")
    check_file(problems, cfg, "./config.schema.json")


def main(argv):
    args, flags, i = {}, set(), 0
    while i < len(argv):
        a = argv[i]
        if a in ("--write", "--check"):
            flags.add(a)
        elif a in ("--main", "--ws", "--flood-from", "--meta", "--campaign") and i + 1 < len(argv):
            args[a[2:]] = argv[i + 1]
            i += 1
        else:
            sys.exit(__doc__)
        i += 1
    if "--check" in flags:
        if args:
            sys.exit("--check takes no option: it checks every committed campaign")
        return check(args)
    cid = args.get("campaign") or latest_id()
    out = generate(args, cid)
    if "--write" in flags:
        path = os.path.join(campaign_dir(cid), "benchmarks.json")
        with open(path, "w") as f:
            f.write(out)
        print(f"data written to {os.path.relpath(path, ROOT)}")
    else:
        sys.stdout.write(out)


if __name__ == "__main__":
    main(sys.argv[1:])
