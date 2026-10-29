#!/usr/bin/env python3
"""Collection of the benchmark results: one JSON document per run directory.

The load generator (bench-client) prints one JSON document per invocation; the
harness scripts take raw snapshots of the machines' counters (cgroup CPU, process
status, process stat, network counters) before and after it. This tool turns
them into the result entry of the run's `run.json` (schema:
bench/data/schema/run.schema.json), including the saturation and validity
verdicts. Bash only passes paths and numbers; nothing reads a line of the generator back.

    record.py init <run dir> --proxy P --transport plain|tls|tlsfull|h2 --matrix http|ws
                   --host docker|remote --auth on|off --repeat N --workers N [--param key=value ...]
    record.py seed <run dir> <seed>
    record.py add  <run dir> --generator gen.json [--generator gen2.json ...] --before DIR --after DIR
                   --repeat N --proxy-cpus SPEC --up-threads N --sink-threads N [--gen-threads N] [--conns N]
    record.py cpus <cpuset>          the number of CPUs of a cpuset such as 0, 0-3 or 0,2-3
    record.py manifest <file> --role main|ws|ktls|scaling|flood --proxy P --transport T --run <run dir> [--cores N]

Snapshot directories hold the raw dumps, named after what they are:
proxy.cpu_stat (cgroup cpu.stat), proxy.status (cat /proc/[0-9]*/status of the
proxy's container, taken after the run), proxy.net (cat /proc/net/snmp
/proc/net/netstat), up.stat and sink.stat (cat /proc/[0-9]*/stat of the origin
and of the sink).

Exit status of `add`: 0 recorded, 3 recorded but the point is INVALID, 4 nothing
to record (the generator printed no document)."""
import argparse, datetime, json, math, os, re, sys

SCHEMA_VERSION = 1
RUN_SCHEMA = "bench/data/schema/run.schema.json"
TCP_FIELDS = (("segs_out_per_run", "OutSegs"), ("active_opens_per_run", "ActiveOpens"),
              ("retrans_per_run", "RetransSegs"), ("delayed_acks_per_run", "DelayedACKs"),
              ("listen_overflows", "ListenOverflows"), ("listen_drops", "ListenDrops"))
PROBE_PROCESSES = ("sh", "cat")  # the shell and cat of the probe itself are not the proxy


# ---- raw counters -----------------------------------------------------------

def cgroup_ticks(cpu_stat):
    """CPU ticks (1/100 s) of a cgroup v2 `cpu.stat` (usage_usec), truncated like `printf %d`."""
    for line in cpu_stat.splitlines():
        if line.startswith("usage_usec"):
            return int(int(line.split()[1]) / 10000)
    raise ValueError("no usage_usec in cpu.stat")


def process_ticks(stat_dump):
    """utime + stime ticks summed over a dump of /proc/<pid>/stat files (one per line)."""
    total = 0
    for line in stat_dump.splitlines():
        after = line.rsplit(")", 1)[-1].split()  # comm may contain spaces and parentheses
        if len(after) >= 13:
            total += int(after[11]) + int(after[12])
    return total


def status_mb(status_dump, key):
    """Sum of a `/proc/<pid>/status` kB field (VmRSS, VmHWM) over the processes, in MiB with one
    decimal, leaving out the probe's own shell and cat."""
    name, total = "", 0
    for line in status_dump.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[0] == "Name:":
            name = parts[1]
        elif len(parts) >= 2 and parts[0] == key + ":" and name not in PROBE_PROCESSES:
            total += int(parts[1])
    return float(f"{total / 1024:.1f}")


def tcp_counters(net_dump):
    """The Tcp / TcpExt counters of `cat /proc/net/snmp /proc/net/netstat`."""
    header, values = {}, {}
    for line in net_dump.splitlines():
        prefix = line.split(" ", 1)[0]
        if prefix not in ("Tcp:", "TcpExt:"):
            continue
        fields = line.split()[1:]
        if prefix not in header:
            header[prefix] = fields
        else:
            values.update(zip(header[prefix], map(int, fields)))
    return {out: values.get(name, 0) for out, name in TCP_FIELDS}


def tcp_delta(before, after, repeat):
    """Per measured run (segments, opens, retransmits, delayed ACKs); accept-queue overflows and
    drops as totals (any above zero explains SYN retransmits)."""
    out = {}
    for out_name, _ in TCP_FIELDS:
        d = after[out_name] - before[out_name]
        out[out_name] = d if out_name.startswith("listen_") else int(d / repeat)
    return out


def cpuset_count(spec):
    """Number of CPUs of a cpuset list such as `0`, `0-3` or `0,2-3`."""
    n = 0
    for part in spec.split(","):
        lo, _, hi = part.partition("-")
        n += int(hi) - int(lo) + 1 if hi else 1
    return n


def per_run(ticks_after, ticks_before, repeat, decimals):
    """CPU seconds attributable to one measured run (the warmup is not in the window)."""
    return float(f"{(ticks_after - ticks_before) / 100 / repeat:.{decimals}f}")


# ---- verdicts ---------------------------------------------------------------

def saturation(elapsed, proxy_cpu, up_cpu, sink_cpu, proxy_cores, up_threads, sink_threads, mode):
    """Did this run reach the proxy's limit? The proxy is at it when its CPU is about 100 % of the
    cores it was given; below, the harness (too few connections, a slow generator, origin or sink)
    or a latency-bound proxy set the number. The origin and the sink are flagged above 90 % of
    their threads."""
    if not elapsed or elapsed <= 0:
        return None
    pu = proxy_cpu / (elapsed * proxy_cores)
    ou = up_cpu / (elapsed * up_threads)
    su = sink_cpu / (elapsed * sink_threads)
    flags = []
    if mode != "hold" and pu < 0.90:
        flags.append("proxy_unsaturated")
    if ou > 0.90:
        flags.append("origin_saturated")
    if su > 0.90:
        flags.append("sink_saturated")
    return {"proxy_util": float(f"{pu:.2f}"), "origin_util": float(f"{ou:.2f}"),
            "sink_util": float(f"{su:.2f}"), "flags": flags}


def generator_flag(gen_cpu, elapsed, gen_threads):
    """A generator at its limit measures itself, not the proxy."""
    return bool(elapsed and elapsed > 0 and gen_cpu / (elapsed * gen_threads) > 0.7)


def problems_of(mode, workload, measured, err_kinds):
    """What makes a point unpublishable: a client-side error, or fewer tunnels than asked (a
    harness or kernel limit, never a property of the proxy)."""
    out = []
    if err_kinds:
        out.append(f"err_kinds={err_kinds}")
    if mode == "hold" and measured.get("opened", 0) < workload.get("conns", 0):
        out.append(f"opened {int(measured['opened'])} of {workload['conns']} tunnels "
                   "(check dmesg for OOM, vm.max_map_count, pids limit)")
    return out


def test_id(mode, workload):
    """Stable name of a point: the workload, not the sweep level."""
    if mode == "http":
        return "http-" + workload["url"].rsplit("/", 1)[1] + ("" if workload["keepalive"] else "-short")
    if mode == "ws":
        if workload["short"]:
            return "ws-short"
        size = workload["size"]
        return "ws-" + (f"{size}b" if size < 1024 else f"{size // 1024}kb")
    if mode == "flood":
        return "flood"
    return f"hold-{workload['conns']}"


# ---- building a result entry ------------------------------------------------

def read(path):
    with open(path, errors="replace") as f:
        return f.read()


def load_json(path):
    with open(path) as f:
        return json.load(f)


def entry_from_generator(doc):
    """The part of a result entry the generator says: workload, err_kinds, measured, generator."""
    workload = dict(doc["workload"])
    mode = workload.pop("mode")
    entry = {"test": test_id(mode, doc["workload"]), "mode": mode, "workload": workload}
    if doc.get("err_kinds") is not None:
        entry["err_kinds"] = doc["err_kinds"]
    entry["measured"] = doc["median"]
    gen = dict(doc["generator"])
    gen["tls"] = doc.get("tls")
    entry["generator"] = gen
    return entry


def combine_replicas(docs, conns):
    """Hold split across client replicas (a single client exhausts the ephemeral ports): the
    tunnels opened and failed add up, the run lasts as long as the slowest replica."""
    opened = sum(int(d["median"]["opened"]) for d in docs)
    failed = sum(int(d["median"]["failed"]) for d in docs)
    elapsed = max(d["median"]["elapsed_s"] for d in docs)
    measured = {"opened": float(opened), "failed": float(failed),
                "rate": float(f"{opened / elapsed:.0f}"), "elapsed_s": elapsed}
    return {"test": f"hold-{conns}", "mode": "hold", "workload": {"conns": conns, "replicas": len(docs)},
            "measured": measured, "generator": {"cpu_s_per_run": None, "own_rss_mb": None,
                                                "own_rss_end_mb": None, "tls": None}}


def build_entry(docs, before, after, repeat, proxy_cores, up_threads, sink_threads, gen_threads=None, conns=None):
    """The result entry of one test. `before` / `after` map a snapshot name to its raw text."""
    replicas = len(docs) > 1
    entry = combine_replicas(docs, conns) if replicas else entry_from_generator(docs[0])
    mode = entry["mode"]
    proxy_cpu = per_run(cgroup_ticks(after["proxy.cpu_stat"]), cgroup_ticks(before["proxy.cpu_stat"]),
                        1 if replicas else repeat, 3)
    proxy = {"cpu_s_per_run": proxy_cpu, "rss_mb": status_mb(after["proxy.status"], "VmRSS"),
             "peak_rss_mb": status_mb(after["proxy.status"], "VmHWM"), "tcp": None}
    sat = None
    if not replicas:
        proxy["tcp"] = tcp_delta(tcp_counters(before["proxy.net"]), tcp_counters(after["proxy.net"]), repeat)
        up = per_run(process_ticks(after["up.stat"]), process_ticks(before["up.stat"]), repeat, 2)
        sink = per_run(process_ticks(after["sink.stat"]), process_ticks(before["sink.stat"]), repeat, 2)
        elapsed = entry["measured"].get("elapsed_s") or entry["workload"].get("duration")
        sat = saturation(elapsed, proxy_cpu, up, sink, proxy_cores, up_threads, sink_threads, mode)
        if sat is not None and gen_threads and generator_flag(entry["generator"]["cpu_s_per_run"], elapsed, gen_threads):
            sat["flags"].append("generator_saturated")
    entry["proxy"], entry["saturation"] = proxy, sat
    problems = problems_of(mode, entry["workload"], entry["measured"], entry.get("err_kinds"))
    entry["valid"], entry["problems"] = not problems, problems
    return entry


# ---- run.json ---------------------------------------------------------------

def run_file(run_dir):
    return os.path.join(run_dir, "run.json")


def write_json(path, doc):
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(doc, f, indent=1, ensure_ascii=False)
        f.write("\n")
    os.replace(tmp, path)


def schema_pointer(run_dir):
    """Relative path from the run directory to the run schema, when the directory is inside the repository."""
    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))  # repo root
    target = os.path.join(root, RUN_SCHEMA)
    return os.path.relpath(target, os.path.abspath(run_dir))


def scalar(value):
    """A header parameter from the command line: numbers and booleans typed, the rest a string."""
    if value in ("true", "false"):
        return value == "true"
    if re.fullmatch(r"-?\d+", value):
        return int(value)
    return value


def init_run(run_dir, proxy, transport, matrix, host, auth, repeat, workers, params, started=None):
    os.makedirs(run_dir, exist_ok=True)
    doc = {"$schema": schema_pointer(run_dir), "schema_version": SCHEMA_VERSION, "proxy": proxy,
           "transport": transport, "matrix": matrix, "host": host, "auth": auth,
           "started": started or datetime.datetime.now().astimezone().strftime("%Y-%m-%dT%H:%M:%S%z"),
           "seed": None, "repeat": repeat, "workers": workers, "params": params, "results": []}
    write_json(run_file(run_dir), doc)


def update_run(run_dir, fn):
    doc = load_json(run_file(run_dir))
    fn(doc)
    write_json(run_file(run_dir), doc)


def snapshot(directory):
    out = {}
    for name in os.listdir(directory):
        out[name] = read(os.path.join(directory, name))
    return out


def summary(entry):
    m = entry["measured"]
    head = f"{entry['test']} conns={entry['workload'].get('conns')}"
    if "rps" in m:
        head += f" rps={m['rps']}"
    if "gbps" in m:
        head += f" gbps={m['gbps']}"
    if "opened" in m:
        head += f" opened={int(m['opened'])}"
    sat = entry["saturation"]
    return f"{head} cpu_s_per_run={entry['proxy']['cpu_s_per_run']}" + (f" proxy_util={sat['proxy_util']}" if sat else "")


def cmd_add(args):
    docs = []
    for path in args.generator:
        try:
            docs.append(load_json(path))
        except (OSError, ValueError):
            pass  # a replica that died: its tunnels are simply not opened
    if not docs:
        print("!! no result document from the generator", file=sys.stderr)
        return 4
    entry = build_entry(docs, snapshot(args.before), snapshot(args.after), args.repeat, cpuset_count(args.proxy_cpus),
                        args.up_threads, args.sink_threads, args.gen_threads, args.conns)
    update_run(args.run_dir, lambda d: d["results"].append(entry))
    print(summary(entry), file=sys.stderr)
    for p in entry["problems"]:
        print(f"!! INVALID {entry['test']}: {p}", file=sys.stderr)
    return 0 if entry["valid"] else 3


def cmd_manifest(args):
    doc = load_json(args.file) if os.path.exists(args.file) else {"schema_version": SCHEMA_VERSION, "runs": []}
    run = {"role": args.role, "proxy": args.proxy, "transport": args.transport}
    if args.cores:
        run["cores"] = args.cores
    run["run"] = args.run
    doc["runs"].append(run)
    write_json(args.file, doc)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("init")
    p.add_argument("run_dir")
    p.add_argument("--proxy", required=True)
    p.add_argument("--transport", required=True, choices=["plain", "tls", "tlsfull", "h2"])
    p.add_argument("--matrix", required=True, choices=["http", "ws"])
    p.add_argument("--host", required=True, choices=["docker", "remote"])
    p.add_argument("--auth", required=True, choices=["on", "off"])
    p.add_argument("--repeat", type=int, required=True)
    p.add_argument("--workers", type=int, required=True)
    p.add_argument("--param", action="append", default=[], metavar="KEY=VALUE")
    p = sub.add_parser("seed")
    p.add_argument("run_dir")
    p.add_argument("seed", type=int)
    p = sub.add_parser("add")
    p.add_argument("run_dir")
    p.add_argument("--generator", action="append", required=True)
    p.add_argument("--before", required=True)
    p.add_argument("--after", required=True)
    p.add_argument("--repeat", type=int, required=True)
    p.add_argument("--proxy-cpus", required=True)
    p.add_argument("--up-threads", type=int, required=True)
    p.add_argument("--sink-threads", type=int, required=True)
    p.add_argument("--gen-threads", type=int)
    p.add_argument("--conns", type=int)
    p = sub.add_parser("cpus")
    p.add_argument("cpuset")
    p = sub.add_parser("manifest")
    p.add_argument("file")
    p.add_argument("--role", required=True, choices=["main", "ws", "ktls", "scaling", "flood"])
    p.add_argument("--proxy", required=True)
    p.add_argument("--transport", required=True)
    p.add_argument("--run", required=True)
    p.add_argument("--cores", type=int)
    args = ap.parse_args(argv)
    if args.cmd == "init":
        params = dict(kv.split("=", 1) for kv in args.param)
        init_run(args.run_dir, args.proxy, args.transport, args.matrix, args.host, args.auth == "on", args.repeat, args.workers,
                 {k: scalar(v) for k, v in params.items()})
    elif args.cmd == "seed":
        update_run(args.run_dir, lambda d: d.__setitem__("seed", args.seed))
    elif args.cmd == "add":
        return cmd_add(args)
    elif args.cmd == "cpus":
        print(cpuset_count(args.cpuset))
    else:
        cmd_manifest(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
