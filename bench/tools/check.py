#!/usr/bin/env python3
"""`make data-check`, the part a JSON Schema cannot express (the files themselves are validated by
`make schema-check`, check-jsonschema): everything published from the numbers is regenerated and
compared with the committed files, and the files are consistent with each other.

    tools/check.py

* benchmarks.json of every campaign is what its manifest and raw runs give (aggregate.py), and the
  charts, the README table and numbers and the tables and numbers of RESULTS.md are what
  benchmarks.json gives (render.py): none of them can drift or be typed by hand;
* ids agree between the folder, index.json and the files, the by-proxy maps hold exactly the
  campaign's proxies, `$schema` pointers are exact, project URLs are https;
* for the latest campaign: proxies.json against FAIRNESS.md (versions) and dut/*/Dockerfile (TLS and
  HTTP/2 support), each chart file exists;
* the kTLS saving and cost quoted by the README and AGENTS.md are the ones of benchmarks.json."""
import json, os, sys
from urllib.parse import urlsplit

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import aggregate, render  # noqa: E402
from aggregate import BENCH, DATA, ROOT, ports_support, load_json, campaign_dir, index_file  # noqa: E402

MINUS = "\u2212"


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
    fairness = open(os.path.join(BENCH, "documentation", "FAIRNESS.md")).read()
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
            for t in bench["tests"]:
                if t["chart"] and not os.path.isfile(os.path.join(BENCH, "charts", t["chart"])):
                    problems.append(f"{cid}/benchmarks.json: test {t['id']}: chart bench/charts/{t['chart']} does not exist")
    if camp is not None and prox is not None:
        try:
            fresh = aggregate.generate(cid)
        except SystemExit as e:
            problems.append(f"{cid}: " + str(e).replace("aggregate: ", "", 1))
            return
        committed = open(os.path.join(cdir, "benchmarks.json")).read() if os.path.isfile(os.path.join(cdir, "benchmarks.json")) else None
        if committed != fresh:
            problems.append(f"{cid}/benchmarks.json is stale: run `python3 bench/tools/aggregate.py --write --campaign {cid}`")


KTLS_FILES = ("README.md", "AGENTS.md")


def check_ktls_wording(problems, bench):
    """The kTLS saving and cost quoted next to the option are the measured ones (ktls[] of benchmarks.json)."""
    ktls = {k["id"]: k for k in bench["ktls"]}
    pct = lambda k: round((ktls[k]["ktls"] / ktls[k]["user_space"] - 1) * 100)
    cost = sorted(-pct(k) for k in ktls if "-ram-" not in k)
    saving = sorted(-pct(k) for k in ktls if "-ram-" in k)
    lo, hi = saving[0], saving[-1]
    for path in KTLS_FILES:
        # comment markers dropped, whitespace collapsed, typographic minus and en dash made plain
        text = " ".join(l.strip().lstrip("/#!").strip() for l in open(os.path.join(ROOT, path)).read().splitlines())
        text = " ".join(text.replace(MINUS, "-").replace("\u2013", " to ").split())
        if not any(p in text for p in (f"{lo} % to {hi} %", f"{lo} to {hi} %")):
            problems.append(f"{path}: the kTLS memory saving must be {lo} % to {hi} % (benchmarks.json ktls[])")
        for want in (f"-{cost[0]} %", f"-{cost[-1]} %"):
            if want not in text:
                problems.append(f"{path}: the kTLS throughput cost must quote {want} (benchmarks.json ktls[])")


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
        latest = index["latest"]
        if not problems:
            stale = [p for p, text in render.render_all(latest).items()
                     if (open(os.path.join(ROOT, p)).read() if os.path.isfile(os.path.join(ROOT, p)) else None) != text]
            if stale:
                problems.append("out of date with benchmarks.json: " + ", ".join(stale) + " (run `make data`)")
            check_ktls_wording(problems, load_json(os.path.join(campaign_dir(latest), "benchmarks.json")))
    check_file(problems, os.path.join(ROOT, "docs", "config.json"), "./config.schema.json")


def main():
    problems = []
    try:
        check_all(problems)
    except (KeyError, TypeError, IndexError) as e:
        problems.append(f"unexpected structure ({e!r}): run `make schema-check` first, it reports the exact file and path")
    except SystemExit as e:
        problems.append(str(e).replace("aggregate: ", "", 1))
    if problems:
        print("\n".join(f"check: {p}" for p in problems), file=sys.stderr)
        sys.exit(1)
    print("data up to date")


if __name__ == "__main__":
    main()
