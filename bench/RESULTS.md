# Résultats — myproxy vs 3proxy

Dates : 2026-09-27/28 · `{myproxy, 3proxy} × auth=on`, médiane de 3 runs
(± warmup), client Docker local (aucun accès Internet).

## Environnement

- Machine de dev : **2 cores, 4 GB RAM** (cœurs exposés "0,2"), Docker + compose.
- Pinning CPU : **proxy → core 0**, client/upstream/sink → core 2. Le débit
  absolu est plafonné par client+sink partageant un core ; les ratios
  myproxy/3proxy restent valides (conditions identiques).
- Upstream : nginx (1kb/10kb/1mb, keep-alive). Sink : serveur discard Rust.
- Sysctls : `somaxconn=65535`, `ip_local_port_range=1024 65535`,
  `tcp_tw_reuse=1` ; `ulimit -n 524288` partout.
- Sources brutes : `bench/results/<proxy>/auth-on/<timestamp>/results.txt`.

## Tableau final (médiane, auth on)

Trois passes myproxy :

- **M5 (avant)** = `copy_bidirectional` tokio, buffers 8 KB alloués d'avance ;
- **M6** = relay adaptatif user-space (buffers 8→64 KB, pages virtuelles) ;
- **M7** = hybride : tunnels et réponses bulk en **`splice()` zéro-copie**,
  petites réponses en copy adaptatif (pas d'overhead sur les petits objets).

| Workload | myproxy M5 | myproxy M6 | **myproxy M7** | 3proxy | Verdict |
|---|---|---|---|---|---|
| HTTP 1 KB, keep-alive ×16 | 10 574 rps | 9 771 rps | **10 028 rps · p50 1,5 ms** | 388 rps · p50 41 ms | **myproxy ×26*** |
| HTTP 10 KB, ×16 | 8 992 rps | 8 208 rps | 8 602 rps (88 MB/s) | **12 499 rps** (128 MB/s) | 3proxy +45 % |
| HTTP 1 MB, ×16 | 604 rps (634 MB/s) | 1 143 rps (1 199 MB/s) | **1 774 rps (1 860 MB/s)** | **2 681 rps** (2 811 MB/s) | 3proxy +51 % ( était ×4,4 → ×1,5) |
| HTTP 1 KB, 1 req/conn ×64 | 4 162 rps | 3 846 rps | **4 174 rps · p50 7,7 ms** | 3 048 rps · p50 15,4 ms | **myproxy +37 %** |
| CONNECT flood ×8 | 4,86 Gbps · 102 % CPU | 15,73 Gbps · 102 % CPU | **17,68 Gbps · 56 % CPU** | **18,67 Gbps · 42 % CPU** | débit ≈ parité (×1,06), CPU gap ÷6 |
| Hold 1 000 tunnels | 0 échec · RSS 20,6 MB | idem | 5 376 conn/s · **RSS 4,9 MB** | 4 098 conn/s · RSS 3,4 MB | parité |
| Hold 10 000 | 1 720 conn/s · RSS 182 MB | 1 614 · 166 MB | **1 911 conn/s · RSS 14,9 MB** | 1 592 conn/s · 3,4 MB | **débit +20 %**, RAM ×4,4 |
| Hold 25 000 | 25 000/25 000 · p99 5,0 s | idem | **25 000/25 000 ×3 · p99 0,84 s · RSS 29,4 MB** | 16 903/25 000 (mur ~17k) | **myproxy seul à tenir** |

## Gains M6+M7 mesurés (médiane vs M5)

- **Tunnel CONNECT : ×3,6 en débit** (4,86 → 17,68 Gbps) et **CPU ÷2** pour
  les obtenir (102 % → 56 % d'un core) : efficiency 0,46 → 3,17 Gbps/core.
- **HTTP 1 MB : ×2,9** (604 → 1 774 rps ; 634 → 1 860 MB/s), CPU/req ÷3.
- **RAM idle** : RSS post-hold 182 → 14,9 MB à 10k tunnels (**÷12**) — le
  relay zéro-copie n'alloue plus rien en user-space.
- **Latence de queue hold** : p99 5,0 s → 0,84 s à 25k tunnels.
- Établissement inchangé et dominant (cf. tableau).

## Conclusions

- **Chemin d'établissement : myproxy domine** — accept+parse+auth+connect :
  ×26 sur les petits objets, +37 % en connexions courtes, latence ÷2, et
  seul à tenir 25 000 tunnels simultanés sur cette machine.
- **Data plane : quasi parité** — débit tunnel ×1,06 sous 3proxy, efficiency
  CPU 3,17 vs 4,4 Gbps/core (gap restant ×1,4, probablement le `F_SETPIPE_SZ`
  / buffering fin de 3proxy). HTTP bulk : ×1,5.
- **RAM idle** : ×4,4 contre 3proxy (architecture task-per-conn de tokio).
  Le prochain cran = pooling o tasks (M8).

## Caveats honnêtes

1. Le 388 req/s de 3proxy sur HTTP 1 KB vient avec p50 = 41 ms ≈
   **delayed-ACK (40 ms)** (suspicion Nagle/write-pattern 3proxy). Le ×26
   est réel dans ces conditions, à vérifier avant publication.
2. 3proxy à 25k : accepte puis ferme sans répondre au-delà de ~17k tunnels
   (config best-effort `maxconn 200000`).
3. Client+sink sur un même core : le plafond absolu du harnais est atteint
   au flood (~18-19 Gbps) ; les deux proxies en buttent au même plafond.
4. 2 cores seulement : matrice 50k/100k et scaling 1/2/4/8 cores à refaire
   sur le VPS cible (même harnais, `PROXY_CPU`/`BENCH_CPU` auto-détectés).
5. RAM mesurée post-test (allocateur libéré, pages non rendues à l'OS) ;
   identique pour les deux proxies.

## Backlog M8 (piloté par ces chiffres)

1. **F_SETPIPE_SZ** sur les pipes de relay (64 KB → 1 MB) +venir à bout du
   gap CPU ×1,4 du tunnel.
2. **Pooling tasks/buffers** : RAM ×4,4 vs 3proxy (1,2 KB/conn → ~0,3).
3. **Multi-worker** (`workers = N`) + `SO_REUSEPORT` + courbe de scaling.
4. Matrice 50k/100k + scaling sur le VPS cible.
5. Vérification du point 3proxy/delayed-ACK (honnêteté du ×26).
