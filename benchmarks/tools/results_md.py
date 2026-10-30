"""benchmarks/documentation/RESULTS.md from the benchmark data: the tables between `<!-- bench:<name>:start -->` and
`<!-- bench:<name>:end -->` markers, and every number of the prose between inline markers

    <!--n EXPR-->value<!--/n-->

where EXPR is evaluated against benchmarks/data/<latest>/benchmarks.json (+ campaign.json, proxies.json)
and the text between the markers is replaced by the result. A number that is a result, or a
condition of the campaign, is never typed: write an empty marker pair and run `make data`.
(Numbers that define the workload, such as the size of an object or a connection count of the
matrix, are inputs and stay in the prose.)

EXPR is a function and its arguments, separated by spaces. References: `<test>.<proxy>[.<field>]`
(a test of benchmarks.json; field = at, cpu_us, cpu_ms_per_gb, p99_ms, proxy_util),
`S<cores>.<proxy>[.<field>]` (a point of the scaling series), `K.<test>.<user_space|ktls>`
(the kTLS comparison), `c.<path>` (campaign.json).

    v REF            the value, formatted like its unit      fmt REF SPEC     Python format spec
    x A B            ratio A/B, one decimal: 2.6×             xi A B           ratio, integer
    pct A B          signed change of A against B: −12 %      apct A B         the same, unsigned
    k REF            thousands: 33.5k                         mem REF          MB, or GB from 1 GiB
    us REF           microseconds: 3,972 µs                   ms REF           microseconds as ms: 4 ms
    at REF           connection count: 4,096                  util REF         0.97 -> 97 %
    xrange T A B..   ratios of A against each B: 1.3× to 1.8×
    kb_per REF N     KiB per unit: memory / N                 gbit1kb REF      req/s of 1 KB -> Gbit/s
    percore REF N R  value / N rounded to R: 55,000           scale P         7.96 (last point / first point)
    ktrange          5 to 20 % (memory saving of kTLS)        ktpct ID         kTLS against user space
    kttp             −12 % to −43 % (throughput cost of kTLS) gb REF           GiB with one decimal
    gbit REF BYTES   req/s of objects of BYTES -> Gbit/s
    sutil / sorigin_peak / sorigin_rest: utilisation of the scaling points
    c PATH           a field of campaign.json"""
import re

MINUS = "−"
MARK = re.compile(r"<!--n (.*?)-->(.*?)<!--/n-->", re.S)


class Ctx:
    def __init__(self, bench, proxies, campaign):
        self.tests = {t["id"]: t for t in bench["tests"]}
        self.scaling = {p["cores"]: p for s in bench["scaling"] for p in s["points"]}
        self.ktls = {k["id"]: k for k in bench["ktls"]}
        self.names = {p["id"]: p["name"] for p in proxies["proxies"]}
        self.order = [p["id"] for p in proxies["proxies"]]
        self.campaign = campaign


# ---- references -------------------------------------------------------------

def ref(ctx, token):
    """(value, unit) of a reference."""
    parts = token.split(".")
    if parts[0] == "K":
        e = ctx.ktls[parts[1]]
        return e[parts[2]], e["unit"]
    if re.fullmatch(r"S\d+", parts[0]):
        point = ctx.scaling[int(parts[0][1:])]
        proxy, field = parts[1], parts[2] if len(parts) > 2 else "values"
        data = point["at_conns" if field == "at" else field]
        return data[proxy], "req/s"
    t = ctx.tests[parts[0]]
    proxy, field = parts[1], parts[2] if len(parts) > 2 else "values"
    value = t["at_conns" if field == "at" else field][proxy]
    if value is None:
        raise ValueError(f"{token}: no value (status {t['status'][proxy]})")
    unit = {"cpu_us": "µs", "cpu_ms_per_gb": "ms/GB", "p99_ms": "ms", "proxy_util": "ratio", "at": "conns"}.get(field, t["unit"])
    return value, unit


def val(ctx, token):
    v, _ = ref(ctx, token)
    if v is None:
        raise ValueError(f"{token}: no value")
    return v


def pct_text(change, signed=True):
    n = round(change)
    if not signed:
        return f"{abs(n)} %"
    return f"{MINUS if n < 0 else '+'}{abs(n)} %" if n else "0 %"


# ---- functions --------------------------------------------------------------

def f_v(ctx, token):
    v, unit = ref(ctx, token)
    if unit in ("req/s", "msg/s", "conns"):
        return f"{v:,.0f}"
    if unit == "Gbit/s":
        return f"{v:.1f}"
    if unit == "MB":
        return f"{v:,.0f} MB"
    if unit == "ms/GB":
        return f"{v:,.0f} ms"
    return f"{v:g}"


def f_mem(ctx, token):
    v = val(ctx, token)
    return f"{v / 1024:.1f} GB" if v >= 1024 else f"{v:.0f} MB"


def f_xrange(ctx, test, a, *others):
    r = [val(ctx, f"{test}.{a}") / val(ctx, f"{test}.{o}") for o in others]
    return f"{min(r):.1f}× to {max(r):.1f}×"


def sutil(ctx, field):
    return [v for p in ctx.scaling.values() for v in p[field].values() if v is not None]


def f_sorigin_peak(ctx):
    return f"{max(sutil(ctx, 'origin_util')) * 100:.0f} %"


def f_sorigin_rest(ctx):
    vals = sorted(sutil(ctx, "origin_util"))
    return f"{vals[-2] * 100:.0f} %"


def f_ktrange(ctx):
    changes = [abs((e["ktls"] / e["user_space"] - 1) * 100) for k, e in ctx.ktls.items() if "-ram-" in k]
    return f"{min(changes):.0f} to {max(changes):.0f} %"


def f_kttp(ctx):
    changes = sorted((e["ktls"] / e["user_space"] - 1) * 100 for k, e in ctx.ktls.items() if "-ram-" not in k)
    return f"{pct_text(changes[-1])} to {pct_text(changes[0])}"


def f_ktpct(ctx, tid):
    e = ctx.ktls[tid]
    return pct_text((e["ktls"] / e["user_space"] - 1) * 100)


def f_scale(ctx, proxy, decimals=2):
    cores = sorted(ctx.scaling)
    return f"{val(ctx, f'S{cores[-1]}.{proxy}') / val(ctx, f'S{cores[0]}.{proxy}'):.{int(decimals)}f}"


FUNCS = {
    "v": f_v,
    "fmt": lambda c, r, spec: format(val(c, r), spec),
    "x": lambda c, a, b: f"{val(c, a) / val(c, b):.1f}×",
    "xi": lambda c, a, b: f"{val(c, a) / val(c, b):.0f}×",
    "pct": lambda c, a, b: pct_text((val(c, a) / val(c, b) - 1) * 100),
    "apct": lambda c, a, b: pct_text((val(c, a) / val(c, b) - 1) * 100, signed=False),
    "k": lambda c, r: f"{val(c, r) / 1000:.1f}k",
    "mem": f_mem,
    "us": lambda c, r: f"{val(c, r):,.0f} µs",
    "ms": lambda c, r: f"{val(c, r) / 1000:.0f} ms",
    "at": lambda c, r: f"{val(c, r + '.at'):,.0f}",
    "util": lambda c, r: f"{val(c, r + '.proxy_util') * 100:.0f} %",
    "xrange": f_xrange,
    "kb_per": lambda c, r, n: f"{val(c, r) * 1024 / float(n):.0f}",
    "gbit1kb": lambda c, r: f"{val(c, r) * 1024 * 8 / 1e9:.1f}",
    "percore": lambda c, r, n, rnd: f"{round(val(c, r) / float(n) / float(rnd)) * int(rnd):,}",
    "scale": f_scale,
    "ktrange": f_ktrange,
    "kttp": f_kttp,
    "gb": lambda c, r: f"{val(c, r) / 1024:.1f}",
    "gbit": lambda c, r, n: f"{val(c, r) * float(n) * 8 / 1e9:.1f}",
    "ktpct": f_ktpct,
    "sutil": lambda c: f"{min(sutil(c, 'proxy_util')):.2f} to {max(sutil(c, 'proxy_util')):.2f}",
    "sorigin_peak": f_sorigin_peak,
    "sorigin_rest": f_sorigin_rest,
}


def evaluate(ctx, expr):
    name, *args = expr.split()
    if name == "c":
        v = ctx.campaign
        for part in args[0].split("."):
            v = v[part]
        return str(v)
    return FUNCS[name](ctx, *args)


def expand(text, ctx):
    return MARK.sub(lambda m: f"<!--n {m.group(1).strip()}-->{evaluate(ctx, m.group(1).strip())}<!--/n-->", text)


# ---- tables -----------------------------------------------------------------

def bold_best(values, direction):
    present = [v for v in values if v is not None]
    return (max(present) if direction == "higher" else min(present)) if present else None


def row(label, cells):
    return "| " + label + " | " + " | ".join(cells) + " |"


def header(ctx, first, cols):
    return [row(first, [ctx.names[c] for c in cols]), "|---|" + "---|" * len(cols)]


def status_cell(t, p):
    return "unsupported" if t["status"][p] == "unsupported" else "—"


def mark(text, is_best):
    return f"**{text}**" if is_best else text


def tests_row(ctx, label, tid, cols, detail, bold=True):
    """A row of a matrix table: the value of each column and a parenthesis from `detail(t, p, first)`."""
    t = ctx.tests[tid]
    best = bold_best([t["values"][c] for c in cols], t["direction"])
    cells = []
    for i, p in enumerate(cols):
        v = t["values"][p]
        if v is None:
            cells.append(status_cell(t, p))
            continue
        text = {"req/s": f"{v:,.0f}", "msg/s": f"{v:,.0f}", "Gbit/s": f"{v:.1f}", "MB": f"{v:,.0f} MB", "ms": f"{v}"}[t["unit"]]
        text = mark(text, bold and v == best)
        extra = detail(t, p, i == 0)
        cells.append(text + (f" ({extra})" if extra else ""))
    return row(label, cells)


def at(t, p):
    return f"@{t['at_conns'][p]}"


def d_us_at(t, p, first):
    return f"{t['cpu_us'][p]:.0f} µs{'/req' if first else ''}, {at(t, p)}"


def d_at(t, p, first):
    return at(t, p)


def d_at_cpu(t, p, first):
    """@conns, flagged when the proxy was under 90 % of its core (the test network or the harness set the number)."""
    return at(t, p) + (", <90 % CPU" if "proxy_unsaturated" in t["flags"][p] else "")


def d_gbps_ms(t, p, first):
    return f"{t['cpu_ms_per_gb'][p]:,.0f} ms/GB, {at(t, p)}"


def ram_rows(ctx, prefix, cols):
    out = []
    for n in (1000, 5000, 10000, 25000):
        out.append(tests_row(ctx, f"RAM, {n:,} idle tunnels", f"{prefix}-ram-{n}", cols, lambda t, p, f: "", bold=n == 25000))
    return out


def matrix_table(ctx, prefix, cols, flood_with_cost, cpu_row):
    rows = header(ctx, "Test", cols)
    rows.append(tests_row(ctx, "HTTP 1 KB, req/s", f"{prefix}-1kb", cols, d_us_at))
    rows.append(tests_row(ctx, "HTTP 10 KB, req/s", f"{prefix}-10kb", cols, d_us_at))
    rows.append(tests_row(ctx, "1 KB, no keep-alive, req/s", f"{prefix}-short", cols, d_us_at))
    rows.append(tests_row(ctx, "HTTP 1 MB, req/s", f"{prefix}-1mb", cols, d_at_cpu))
    rows.append(tests_row(ctx, "HTTP 10 MB, req/s", f"{prefix}-10mb", cols, d_at_cpu))
    rows.append(tests_row(ctx, "Tunnels (flood), Gbit/s", f"{prefix}-flood", cols, d_gbps_ms if flood_with_cost else d_at_cpu))
    if cpu_row:
        t = ctx.tests[f"{prefix}-flood"]
        best = bold_best([t["cpu_ms_per_gb"][c] for c in cols], "lower")
        rows.append(row("Tunnels, proxy CPU per GB", [mark(f"{t['cpu_ms_per_gb'][c]:,.0f} ms", t["cpu_ms_per_gb"][c] == best) for c in cols]))
    rows += ram_rows(ctx, prefix, cols)
    return "\n".join(rows)


def p99_text(v):
    return f"{v:.2f} ms" if v < 5 else f"{v:.1f} ms"


def ws_table(ctx, prefix, cols, with_cpu):
    def detail_rate(t, p, first):
        parts = [("p99 " if first else "") + p99_text(t["p99_ms"][p])]
        if with_cpu:
            parts.append(f"{t['cpu_us'][p]:,.0f} µs{'/msg' if first and t['id'].endswith('64b-rps') else ''}")
        return ", ".join(parts)

    def detail_gb(t, p, first):
        return f"{t['cpu_ms_per_gb'][p]:,.0f} ms/GB"

    rows = header(ctx, "Test", cols)
    rows.append(tests_row(ctx, "64 B messages, msg/s", f"{prefix}-64b-rps", cols, detail_rate))
    rows.append(tests_row(ctx, "64 KB frames, Gbit/s", f"{prefix}-64kb-gbps", cols, detail_gb))
    rows.append(tests_row(ctx, "One connection per exchange, msg/s", f"{prefix}-short", cols, detail_rate))
    return "\n".join(rows)


def ktls_table(ctx):
    rows = ["| Test | rustle, TLS in user space | rustle, kTLS | Change |", "|---|---|---|---|"]
    for tid, label in (("tls-1kb", "HTTP 1 KB, req/s"), ("tls-10kb", "HTTP 10 KB, req/s"), ("tls-short", "1 KB, no keep-alive, req/s"),
                       ("tls-1mb", "HTTP 1 MB, req/s"), ("tls-10mb", "HTTP 10 MB, req/s")):
        e = ctx.ktls[tid]
        rows.append(row(label, [f"{e['user_space']:,.0f}", f"{e['ktls']:,.0f}", f_ktpct(ctx, tid)]))
    e = ctx.ktls["tls-flood"]
    cost = (e["ktls_cpu_ms_per_gb"] / e["user_space_cpu_ms_per_gb"] - 1) * 100
    rows.append(row("Tunnels, Gbit/s", [f"{e['user_space']:.1f} ({e['user_space_cpu_ms_per_gb']:,.0f} ms/GB)",
                                        f"{e['ktls']:.1f} ({e['ktls_cpu_ms_per_gb']:,.0f} ms/GB)",
                                        f"{f_ktpct(ctx, 'tls-flood')} ({pct_text(cost)} CPU per GB)"]))
    for n in (1000, 5000, 10000, 25000):
        e = ctx.ktls[f"tls-ram-{n}"]
        rows.append(row(f"RAM, {n:,} idle tunnels", [f"{e['user_space']:,.0f} MB", f"{e['ktls']:,.0f} MB", f_ktpct(ctx, f"tls-ram-{n}")]))
    return "\n".join(rows)


def scaling_table(ctx):
    cols = [c for c in ctx.order if any(p["values"][c] is not None for p in ctx.scaling.values())]
    rows = header(ctx, "Proxy cores", cols)
    for cores in sorted(ctx.scaling):
        pt = ctx.scaling[cores]
        best = bold_best([pt["values"][c] for c in cols], "higher")
        rows.append(row(str(cores), [mark(f"{pt['values'][c]:,.0f}", pt["values"][c] == best) + f" (@{pt['at_conns'][c]:,})" for c in cols]))
    rows.append(row("Scaling 1 → 8 cores", [f"×{f_scale(ctx, c)}" for c in cols]))
    return "\n".join(rows)


def environment_table(ctx):
    h = ctx.campaign["hardware"]
    rows = ["| Role | Server | CPU | Role in the run |", "|---|---|---|---|"]
    for label, key in (("Generator", "generator"), ("Proxy", "proxy"), ("Origin + sink", "origin_sink")):
        m = h[key]
        rows.append(row(label, [m["server"], f"{m['cpu']}, {m['cores']} cores / {m['threads']} threads", m["role"]]))
    return "\n".join(rows)


def blocks(ctx):
    plain = ["rustle", "3proxy", "squid", "tinyproxy", "gost"]
    return {
        "environment": environment_table(ctx),
        "plain": matrix_table(ctx, "plain", plain, False, True),
        "tls": matrix_table(ctx, "tls", ["rustle", "squid", "gost"], True, False),
        "h2": matrix_table(ctx, "h2", ["rustle", "gost"], True, False),
        "ws": ws_table(ctx, "ws", ["rustle", "squid", "gost", "3proxy", "tinyproxy"], True),
        "ws-tls": ws_table(ctx, "ws-tls", ["rustle", "gost", "squid"], False),
        "ktls": ktls_table(ctx),
        "scaling": scaling_table(ctx),
    }


def expand_text(text, bench, proxies, campaign):
    """Only the inline markers (README.md)."""
    return expand(text, Ctx(bench, proxies, campaign))


def render(text, bench, proxies, campaign):
    ctx = Ctx(bench, proxies, campaign)
    for name, body in blocks(ctx).items():
        a, b = f"<!-- bench:{name}:start -->", f"<!-- bench:{name}:end -->"
        if a not in text or b not in text:
            raise SystemExit(f"results_md: markers {a} / {b} not found in RESULTS.md")
        i, j = text.index(a) + len(a), text.index(b)
        text = text[:i] + "\n\n" + body + "\n\n" + text[j:]
    return expand(text, ctx)
