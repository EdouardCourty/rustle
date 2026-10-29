"""Unit tests of aggregate.py and the renderers, on rows and figures taken from the 2026-10-28 campaign."""
import json, os, sys, unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import aggregate, render, results_md  # noqa: E402


def result(test="http-1kb", conns=64, rps=55257.595, valid=True, flags=(), cpu=15.0, util=1.0):
    return {"test": test, "mode": "http", "workload": {"url": "http://h/1kb", "conns": conns, "keepalive": True},
            "err_kinds": "" if valid else "status-426=2", "measured": {"rps": rps, "elapsed_s": 15.001},
            "generator": {"cpu_s_per_run": 1.0, "own_rss_mb": 1.0, "own_rss_end_mb": 2.0, "tls": None},
            "proxy": {"cpu_s_per_run": cpu, "rss_mb": 7.1, "peak_rss_mb": 7.1, "tcp": None},
            "harness": {"up_cpu_s_per_run": 1.0, "sink_cpu_s_per_run": None},
            "saturation": {"proxy_util": util, "origin_util": 0.09, "sink_util": 0.0, "flags": list(flags)},
            "valid": valid, "problems": [] if valid else ["err_kinds=status-426=2"]}


class BestLevel(unittest.TestCase):
    def test_the_best_clean_level_wins(self):
        rows = aggregate.best_rows([result(conns=64, rps=1000.0), result(conns=256, rps=3000.0), result(conns=1024, rps=2000.0)])
        self.assertEqual((rows["http-1kb"]["rps"], rows["http-1kb"]["conns_level"]), (3000.0, 256))

    def test_a_faster_level_with_errors_never_beats_a_clean_one(self):
        rows = aggregate.best_rows([result(conns=64, rps=1000.0), result(conns=256, rps=9000.0, valid=False)])
        self.assertEqual((rows["http-1kb"]["rps"], rows["http-1kb"]["valid"]), (1000.0, True))

    def test_the_first_of_equals_wins(self):
        rows = aggregate.best_rows([result(conns=64, rps=1000.0), result(conns=256, rps=1000.0)])
        self.assertEqual(rows["http-1kb"]["conns_level"], 64)

    def test_cpu_per_request(self):
        row = aggregate.best_rows([result(rps=55257.595, cpu=15.0)])["http-1kb"]
        self.assertAlmostEqual(row["us"], 15.0 / (55257.595 * 15.001) * 1e6)  # 18.1 µs, as published


class Cells(unittest.TestCase):
    support = {"rustle": {"tls": True, "h2": True}, "3proxy": {"tls": False, "h2": False}}

    def cell(self, data, proxy, transport="tls", unsupported=None):
        return aggregate.cell(data, self.support, unsupported or {}, transport, proxy, "http-1kb", "rps")

    def test_a_clean_point(self):
        data = {"tls": {"rustle": aggregate.best_rows([result(flags=["proxy_unsaturated"])])}}
        self.assertEqual(self.cell(data, "rustle"), (55257.595, 64, ["proxy_unsaturated"], "ok"))

    def test_a_proxy_without_a_listener_is_unsupported_not_zero(self):
        self.assertEqual(self.cell({"tls": {}}, "3proxy")[0::3], (None, "unsupported"))

    def test_a_proxy_that_could_but_was_not_run_is_not_measured(self):
        self.assertEqual(self.cell({"tls": {}}, "rustle")[0::3], (None, "not_measured"))

    def test_an_invalid_point_has_no_value(self):
        data = {"tls": {"rustle": aggregate.best_rows([result(valid=False)])}}
        self.assertEqual(self.cell(data, "rustle")[0::3], (None, "invalid"))

    def test_the_campaign_can_declare_a_proxy_unsupported(self):
        data = {"plain": {"tinyproxy": aggregate.best_rows([result(valid=False)])}}
        got = aggregate.cell(data, self.support, {"ws": ["tinyproxy"]}, "ws", "tinyproxy", "http-1kb", "rps")
        self.assertEqual(got[0::3], (None, "unsupported"))


class Published(unittest.TestCase):
    def test_the_published_campaign_is_what_its_raw_runs_give(self):
        cid = aggregate.latest_id()
        committed = open(os.path.join(aggregate.campaign_dir(cid), "benchmarks.json")).read()
        self.assertEqual(aggregate.generate(cid), committed)

    def test_the_charts_are_what_the_data_gives(self):
        cid = aggregate.latest_id()
        for path, svg in render.render_all(cid).items():
            if path.endswith(".svg"):
                self.assertEqual(open(os.path.join(aggregate.ROOT, path)).read(), svg, path)


class Placeholders(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        d = aggregate.campaign_dir(aggregate.latest_id())
        load = lambda n: json.load(open(os.path.join(d, n)))
        cls.ctx = results_md.Ctx(load("benchmarks.json"), load("proxies.json"), load("campaign.json"))

    def ev(self, expr):
        return results_md.evaluate(self.ctx, expr)

    def test_ratios_and_changes(self):
        self.assertEqual(self.ev("x plain-1kb.rustle plain-1kb.3proxy"), "2.6×")
        self.assertEqual(self.ev("pct h2-flood.rustle h2-flood.gost"), "+12 %")
        self.assertEqual(self.ev("apct h2-1kb.rustle tls-1kb.rustle"), "32 %")
        self.assertEqual(self.ev("ktpct tls-flood"), "−43 %")

    def test_values_and_ranges(self):
        self.assertEqual(self.ev("v plain-ram-25000.rustle"), "95 MB")
        self.assertEqual(self.ev("v plain-flood.rustle"), "22.0")
        self.assertEqual(self.ev("xrange tls-short-full rustle gost squid"), "2.4× to 2.8×")
        self.assertEqual(self.ev("percore S8.rustle 8 1000"), "55,000")
        self.assertEqual(self.ev("ktrange"), "5 to 20 %")
        self.assertEqual(self.ev("kttp"), "−12 % to −43 %")

    def test_the_campaign_and_the_conditions(self):
        self.assertEqual(self.ev("c seed"), "2004113323")
        self.assertEqual(self.ev("at S8.3proxy"), "4,096")

    def test_a_missing_number_is_an_error_not_a_blank(self):
        with self.assertRaises(ValueError):
            self.ev("v tls-1kb.3proxy")

    def test_markers_are_rewritten_in_place(self):
        text = "a <!--n x plain-1kb.rustle plain-1kb.3proxy-->WRONG<!--/n--> b <!--n c repeats--><!--/n-->"
        self.assertEqual(results_md.expand(text, self.ctx),
                         "a <!--n x plain-1kb.rustle plain-1kb.3proxy-->2.6×<!--/n--> b <!--n c repeats-->3<!--/n-->")


if __name__ == "__main__":
    unittest.main()
