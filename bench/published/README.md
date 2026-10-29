# Published raw results

The raw measurements behind the numbers in [`../RESULTS.md`](../RESULTS.md) and the README, one folder per campaign.

## 2026-10-28

Three Scaleway Elastic Metal servers (generator and proxy EM-I320E, origin and sink EM-I220E, 25 Gbit/s private network), one proxy core unless stated, seed `2004113323`. Environment and protocol: [`../RESULTS.md`](../RESULTS.md). Machines compared and links measured before the run: [`2026-10-28/preflight.txt`](2026-10-28/preflight.txt).

The text indexes the runs were originally collected with, in `2026-10-28/runs/` (kept as provenance; the manifest that drives the aggregation is the `sources` of [`../data/2026-10-28/campaign.json`](../data/2026-10-28/campaign.json)):

| Index | Content |
|---|---|
| `campaign-20261028-235500.txt` | plain, TLS, TLS full handshake and HTTP/2 matrices (chapters 1 to 3) |
| `campaign-ws-20261028-214800.txt` | WebSocket matrix, plain and TLS (chapter 4) |
| `campaign-20261028-215104.txt` | rustle with kTLS (chapter 5) |
| `campaign-mc-n1.txt` … `campaign-mc-n8.txt` | multi-core curve, `http-1kb`, n = 1, 2, 4, 8 proxy cores (chapter 6) |

Each run is a directory `runs/<proxy>[-tls|-tlsfull|-h2][-ws]/remote-auth-on/<timestamp>/` with `run.json` (the raw result of every test and connection level: measurements, proxy CPU, memory, TCP counters, saturation flags, validity; schema `../data/schema/run.schema.json`) and `results.txt`, the text the run was first written as, from which `run.json` was converted once (the converter is not kept: `run.json` is the format the harness now writes). Only the runs the manifest points to are kept.

Rebuild `benchmarks.json` and everything written from it, and list the points that are not clean, from the repository root:

```sh
make data
python3 bench/tools/aggregate.py --report
```

To reproduce the measurements, follow [`../RUNBOOK.md`](../RUNBOOK.md).
