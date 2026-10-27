# Published raw results

The raw measurements behind the numbers in [`../RESULTS.md`](../RESULTS.md) and the README, one folder per campaign.

## 2026-10-28

Three Scaleway Elastic Metal servers (generator and proxy EM-I320E, origin and sink EM-I220E, 25 Gbit/s private network), one proxy core unless stated, seed `2004113323`. Environment and protocol: [`../RESULTS.md`](../RESULTS.md). Machines compared and links measured before the run: [`2026-10-28/preflight.txt`](2026-10-28/preflight.txt).

Indexes (one line per run: `<proxy> <mode> <path to its results.txt>`), in `2026-10-28/runs/`:

| Index | Content |
|---|---|
| `campaign-20261028-235500.txt` | plain, TLS, TLS full handshake and HTTP/2 matrices (chapters 1 to 3) |
| `campaign-ws-20261028-214800.txt` | WebSocket matrix, plain and TLS (chapter 4) |
| `campaign-20261028-215104.txt` | rustle with kTLS (chapter 5) |
| `campaign-mc-n1.txt` … `campaign-mc-n8.txt` | multi-core curve, `http-1kb`, n = 1, 2, 4, 8 proxy cores (chapter 6) |

Each run is `runs/<proxy>[-tls|-tlsfull|-h2][-ws]/remote-auth-on/<timestamp>/results.txt`: the run's parameters on the first line, then one `RESULT` line per test and connection level (requests per second, latencies, proxy CPU, memory, TCP counters, saturation flags). Only the runs the indexes point to are kept; runs that were stopped by an incident and re-run are described in `RESULTS.md`.

Rebuild the tables and the validity check from the repository root:

```sh
cd bench
./tools/campaign-table.py published/2026-10-28/runs/campaign-20261028-235500.txt
./tools/validity.py published/2026-10-28/runs/campaign-20261028-235500.txt
```

To reproduce the measurements, follow [`../RUNBOOK.md`](../RUNBOOK.md).
