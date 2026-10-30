"""Unit tests of record.py, on fixtures built from the real 2026-10-28 campaign: the raw counters
are chosen so that the derived figures are the published ones (rustle, plain, http-1kb, 64
connections: cpu_s_per_run 15.000, proxy_util 1.00, origin_util 0.09)."""
import json, os, sys, tempfile, unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import record  # noqa: E402

GEN_HTTP = {
    "schema_version": 1,
    "workload": {"mode": "http", "url": "http://172.16.12.2:8000/1kb", "conns": 64, "budget": "15s", "keepalive": True},
    "err_kinds": "",
    "median": {"rps": 55257.595, "mbps": 56.584, "elapsed_s": 15.001, "p50_ms": 1.139, "p95_ms": 1.286,
               "p99_ms": 1.368, "errors": 0.0, "reconnects": 0.0},
    "generator": {"cpu_s_per_run": 11.62, "own_rss_mb": 3.8, "own_rss_end_mb": 7.8},
    "tls": None,
}


def proc_stat(ticks):
    return f"1 (up) S 0 0 0 0 0 0 0 0 0 0 {ticks - 7} 7 0 0\n"


def net(segs, opens, retrans, delayed, overflows, drops):
    return (f"Tcp: RtoAlgorithm ActiveOpens OutSegs RetransSegs\nTcp: 1 {opens} {segs} {retrans}\n"
            f"TcpExt: DelayedACKs ListenOverflows ListenDrops\nTcpExt: {delayed} {overflows} {drops}\n")


STATUS = "Name:\trustle\nVmHWM:\t7270 kB\nVmRSS:\t7270 kB\nName:\tsh\nVmRSS:\t900 kB\nVmHWM:\t900 kB\nName:\tcat\nVmRSS:\t90 kB\n"


def snaps(proxy_s=15.0, up_s=10.24, repeat=3):
    before = {"proxy.cpu_stat": "usage_usec 100000000\nuser_usec 1\n", "proxy.net": net(1000, 10, 1, 5, 0, 0),
              "up.stat": proc_stat(500), "sink.stat": proc_stat(0)}
    after = {"proxy.cpu_stat": f"usage_usec {100000000 + int(proxy_s * repeat * 1e6)}\n",
             "proxy.net": net(1000 + 1658935 * repeat, 10 + 5 * repeat, 1, 5 + 452 * repeat, 0, 0),
             "up.stat": proc_stat(500 + int(up_s * repeat * 100)), "sink.stat": proc_stat(0), "proxy.status": STATUS}
    return before, after


class RawCounters(unittest.TestCase):
    def test_cgroup_ticks_truncate_like_printf(self):
        self.assertEqual(record.cgroup_ticks("usage_usec 12345678\n"), 1234)

    def test_process_ticks_survive_odd_names(self):
        dump = "1 (a b) c) S 0 0 0 0 0 0 0 0 0 0 10 5 0\n2 (x) S 0 0 0 0 0 0 0 0 0 0 1 2 0\n"
        self.assertEqual(record.process_ticks(dump), 18)

    def test_status_leaves_out_the_probe_processes(self):
        self.assertEqual(record.status_mb(STATUS, "VmRSS"), 7.1)
        self.assertEqual(record.status_mb(STATUS, "VmHWM"), 7.1)

    def test_tcp_counters_and_delta(self):
        a, b = record.tcp_counters(net(1000, 10, 1, 5, 0, 0)), record.tcp_counters(net(7000, 25, 4, 20, 2, 1))
        self.assertEqual(record.tcp_delta(a, b, 3), {"segs_out_per_run": 2000, "active_opens_per_run": 5,
                                                     "retrans_per_run": 1, "delayed_acks_per_run": 5,
                                                     "listen_overflows": 2, "listen_drops": 1})


class Cpuset(unittest.TestCase):
    def test_counts(self):
        self.assertEqual([record.cpuset_count(s) for s in ("0", "0-3", "0,2-3", "4-7,10")], [1, 4, 3, 5])


class Entry(unittest.TestCase):
    def build(self, gen=GEN_HTTP, **kw):
        before, after = snaps(**kw)
        return record.build_entry([gen], before, after, 3, 1, 8, 8)

    def test_published_point_is_reproduced(self):
        e = self.build()
        self.assertEqual(e["test"], "http-1kb")
        self.assertEqual(e["proxy"]["cpu_s_per_run"], 15.0)
        self.assertEqual((e["proxy"]["rss_mb"], e["proxy"]["peak_rss_mb"]), (7.1, 7.1))
        self.assertEqual(e["proxy"]["tcp"]["segs_out_per_run"], 1658935)
        self.assertEqual(e["proxy"]["tcp"]["delayed_acks_per_run"], 452)
        self.assertEqual({k: v for k, v in e["saturation"].items() if k != "flags"},
                         {"proxy_util": 1.0, "origin_util": 0.09, "sink_util": 0.0})
        self.assertEqual(e["saturation"]["flags"], [])
        self.assertTrue(e["valid"])
        self.assertEqual(e["workload"], {"url": "http://172.16.12.2:8000/1kb", "conns": 64, "budget": "15s", "keepalive": True})

    def test_a_proxy_under_90_percent_of_its_core_is_flagged(self):
        e = self.build(proxy_s=10.4)  # the 10 MB point of the campaign: 0.69
        self.assertEqual(e["saturation"]["proxy_util"], 0.69)
        self.assertEqual(e["saturation"]["flags"], ["proxy_unsaturated"])

    def test_a_busy_origin_is_flagged(self):
        e = self.build(up_s=120.0)
        self.assertEqual(e["saturation"]["flags"], ["origin_saturated"])

    def test_client_errors_make_the_point_invalid(self):
        e = self.build(gen=dict(GEN_HTTP, err_kinds="status-426=2296"))
        self.assertFalse(e["valid"])
        self.assertEqual(e["problems"], ["err_kinds=status-426=2296"])

    def test_missing_tunnels_make_a_hold_point_invalid(self):
        gen = {"workload": {"mode": "hold", "target": "172.16.12.2:9000", "conns": 10000, "duration": 10, "ramp": 4096},
               "median": {"opened": 9990.0, "failed": 10.0, "rate": 13986.014, "open_s": 0.7, "elapsed_s": 10.035},
               "generator": {"cpu_s_per_run": 1.45, "own_rss_mb": 3.8, "own_rss_end_mb": 36.0}, "tls": None}
        e = self.build(gen=gen)
        self.assertEqual(e["test"], "hold-10000")
        self.assertFalse(e["valid"])
        self.assertIn("opened 9990 of 10000 tunnels", e["problems"][0])
        self.assertNotIn("proxy_unsaturated", e["saturation"]["flags"])  # a hold is not meant to load the CPU

    def test_flood_uses_its_duration_for_the_utilisation(self):
        gen = {"workload": {"mode": "flood", "target": "172.16.12.2:9000", "conns": 8, "duration": 10, "chunk": 65536},
               "median": {"gbps": 22.029, "mb": 27535.831, "errors": 0.0},
               "generator": {"cpu_s_per_run": 4.57, "own_rss_mb": 3.8, "own_rss_end_mb": 7.1}, "tls": None}
        e = self.build(gen=gen, proxy_s=6.97)
        self.assertEqual(e["test"], "flood")
        self.assertEqual(e["saturation"]["proxy_util"], 0.7)
        self.assertEqual(e["saturation"]["flags"], ["proxy_unsaturated"])

    def test_websocket_test_ids(self):
        self.assertEqual(record.test_id("ws", {"short": False, "size": 64}), "ws-64b")
        self.assertEqual(record.test_id("ws", {"short": False, "size": 65536}), "ws-64kb")
        self.assertEqual(record.test_id("ws", {"short": True, "size": 64}), "ws-short")
        self.assertEqual(record.test_id("http", {"url": "http://h/1kb", "keepalive": False}), "http-1kb-short")

    def test_replicas_add_up(self):
        def rep(opened, elapsed):
            return {"workload": {"mode": "hold", "conns": 20000, "duration": 10, "ramp": 4096},
                    "median": {"opened": float(opened), "failed": 0.0, "elapsed_s": elapsed},
                    "generator": {"cpu_s_per_run": 1.0, "own_rss_mb": 1.0, "own_rss_end_mb": 2.0}, "tls": None}
        before, after = snaps()
        e = record.build_entry([rep(20000, 12.0), rep(20000, 12.5)], before, after, 1, 1, 8, 8, conns=40000)
        self.assertEqual((e["test"], e["measured"]["opened"], e["measured"]["elapsed_s"]), ("hold-40000", 40000.0, 12.5))
        self.assertEqual(e["measured"]["rate"], 3200.0)
        self.assertIsNone(e["saturation"])
        self.assertTrue(e["valid"])


class Files(unittest.TestCase):
    def test_a_run_accumulates_results(self):
        with tempfile.TemporaryDirectory() as d:
            run = os.path.join(d, "run")
            record.main(["init", run, "--proxy", "rustle", "--transport", "plain", "--matrix", "http", "--host", "remote", "--auth", "on",
                         "--repeat", "3", "--workers", "1", "--param", "proxy_cpus=0"])
            record.main(["seed", run, "2004113323"])
            before, after = snaps()
            for name, snap in (("before", before), ("after", after)):
                os.makedirs(os.path.join(d, name))
                for k, v in snap.items():
                    open(os.path.join(d, name, k), "w").write(v)
            gen = os.path.join(d, "gen.json")
            json.dump(GEN_HTTP, open(gen, "w"))
            argv = ["add", run, "--generator", gen, "--before", os.path.join(d, "before"), "--after", os.path.join(d, "after"),
                    "--repeat", "3", "--proxy-cpus", "0", "--up-threads", "8", "--sink-threads", "8"]
            self.assertEqual(record.main(argv), 0)
            self.assertEqual(record.main(argv), 0)
            self.assertEqual(record.main(["add", run, "--generator", os.path.join(d, "missing.json")] + argv[4:]), 4)
            doc = json.load(open(os.path.join(run, "run.json")))
            self.assertEqual((doc["seed"], doc["params"], len(doc["results"])), (2004113323, {"proxy_cpus": 0}, 2))


if __name__ == "__main__":
    unittest.main()
