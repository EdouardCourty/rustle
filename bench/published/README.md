# Published raw results

The raw measurements behind the numbers in [`../RESULTS.md`](../documentation/RESULTS.md) and the README, one folder per campaign.

## 2026-10-28

Three Scaleway Elastic Metal servers (generator and proxy EM-I320E, origin and sink EM-I220E, 25 Gbit/s private network), one proxy core unless stated, seed `2004113323`. Environment and protocol: [`../RESULTS.md`](../documentation/RESULTS.md). Machines compared and links measured before the run: [`2026-10-28/preflight.txt`](2026-10-28/preflight.txt).

The manifest of the runs is the `sources` of [`../data/2026-10-28/campaign.json`](../data/2026-10-28/campaign.json).

Each run is a directory `runs/<proxy>[-tls|-tlsfull|-h2][-ws]/remote-auth-on/<timestamp>/` with `run.json`: the raw result of every test and connection level (measurements, proxy CPU, memory, TCP counters, saturation flags, validity; schema `../data/schema/run.schema.json`). Only the runs the manifest points to are kept.

Rebuild `benchmarks.json` and everything written from it, and list the points that are not clean, from the repository root:

```sh
make data
python3 bench/tools/aggregate.py --report
```

To reproduce the measurements, follow [`../RUNBOOK.md`](../documentation/RUNBOOK.md).
