"""run-remote.sh end to end, against a fake `ssh`: the real script, the real matrix.sh and the real
record.py, with the machines' answers made up (counters that grow by a known amount, the
generator's JSON document of a published point). It proves the plumbing: what the script passes to
record.py, the files it leaves, the exit status. It does not measure anything."""
import json, os, shutil, stat, subprocess, sys, tempfile, unittest

BENCH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..")
GEN = {"schema_version": 1,
       "workload": {"mode": "http", "url": "http://c:8000/1kb", "conns": 64, "budget": "15s", "keepalive": True},
       "err_kinds": "", "median": {"rps": 55257.595, "mbps": 56.584, "elapsed_s": 15.001, "p50_ms": 1.139, "p95_ms": 1.286,
                                   "p99_ms": 1.368, "errors": 0.0, "reconnects": 0.0},
       "generator": {"cpu_s_per_run": 11.62, "own_rss_mb": 3.8, "own_rss_end_mb": 7.8}, "tls": None}

FAKE_SSH = r"""#!/usr/bin/env bash
cmd="${@: -1}"; n_file="$FAKE_STATE/calls"
bump() { local n; n=$(cat "$FAKE_STATE/$1" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$FAKE_STATE/$1"; echo "$n"; }
case "$cmd" in
    nproc*) echo 8 ;;
    *"cgroup/cpu.stat"*) n=$(bump cpu); echo "usage_usec $((n * 15000000))" ;;
    *"/proc/net/snmp"*) n=$(bump net); echo "Tcp: ActiveOpens OutSegs RetransSegs"; echo "Tcp: $((n * 5)) $((n * 1658935)) 0"
                        echo "TcpExt: DelayedACKs ListenOverflows ListenDrops"; echo "TcpExt: $((n * 452)) 0 0" ;;
    *"docker exec upstream"*) n=$(bump up); echo "1 (nginx) S 0 0 0 0 0 0 0 0 0 0 $((n * 1024)) 0 0" ;;
    *"docker exec sink"*) echo "1 (sink) S 0 0 0 0 0 0 0 0 0 0 0 0 0" ;;
    *"/proc/[0-9]*/status"*) printf 'Name:\trustle\nVmHWM:\t7270 kB\nVmRSS:\t7270 kB\n' ;;
    *"--warmup-only"*) : ;;
    *bench-client*"--repeat"*) cat "$FAKE_STATE/gen.json" ;;
    *) : ;;
esac
"""


class RemoteRun(unittest.TestCase):
    @unittest.skipUnless(shutil.which("bash"), "needs bash")
    def test_run_remote_records_a_valid_result(self):
        with tempfile.TemporaryDirectory() as tmp:
            os.makedirs(os.path.join(tmp, "bin"))
            ssh = os.path.join(tmp, "bin", "ssh")
            open(ssh, "w").write(FAKE_SSH)
            os.chmod(ssh, os.stat(ssh).st_mode | stat.S_IEXEC)
            json.dump(GEN, open(os.path.join(tmp, "gen.json"), "w"))
            env = dict(os.environ, PATH=os.path.join(tmp, "bin") + os.pathsep + os.environ["PATH"], FAKE_STATE=tmp,
                       GEN_HOST="g", PROXY_HOST="p", UP_HOST="u", PROXY_CPUS="0", BENCH_ONLY="http-1kb", NO_SWEEP="1",
                       REPEAT="1", SKIP_BUILD="1", GEN_THREADS="24", UP_THREADS="8", SINK_THREADS="8")
            proc = subprocess.run(["bash", os.path.join(BENCH, "run-remote.sh"), "rustle", "on"], env=env,
                                  capture_output=True, text=True, timeout=120)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            run_dir = next(l[len("==> run in "):] for l in proc.stdout.splitlines() if l.startswith("==> run in "))
            try:
                doc = json.load(open(os.path.join(BENCH, run_dir, "run.json")))
                self.assertEqual((doc["proxy"], doc["transport"], doc["host"], doc["auth"], doc["seed"], doc["workers"]),
                                 ("rustle", "plain", "remote", True, 2004113323, 1))
                self.assertEqual(len(doc["results"]), 1)
                r = doc["results"][0]
                self.assertEqual((r["test"], r["valid"]), ("http-1kb", True))
                self.assertEqual((r["proxy"]["cpu_s_per_run"], r["proxy"]["peak_rss_mb"]), (15.0, 7.1))
                self.assertEqual(r["saturation"]["proxy_util"], 1.0)
                self.assertEqual(r["proxy"]["tcp"]["segs_out_per_run"], 1658935)
            finally:
                shutil.rmtree(os.path.join(BENCH, run_dir), ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
