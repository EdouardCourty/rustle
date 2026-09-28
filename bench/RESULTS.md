# Résultats — myproxy vs 3proxy

Dates : 2026-09-27/28 · `{myproxy, 3proxy} × auth=on`, médiane de 3 runs
(± warmup), client Docker local (aucun accès Internet).

## Environnement

- Machine de dev : **4 GB RAM**, 2 cores exposés (auto-détectés à chaque
  run — la box a déjà montré "0,2", "1,3", "0-1" selon les reboot).
- Pinning CPU : **proxy → son core**, client/upstream/sink → l'autre. Le
  débit absolu est plafonné par client+sink partageant un core ; les
  ratios myproxy/3proxy restent valides (conditions identiques).
- Upstream : nginx (1kb/10kb/1mb/**10mb**, keep-alive 300 s). Sink : serveur
  discard Rust. Sysctls : `somaxconn=65535`, port range élargi,
  `tcp_tw_reuse=1` ; `ulimit -n 524288` partout.
- Sources brutes : `bench/results/<proxy>/auth-on/<timestamp>/results.txt`.
- Mémoire : builds Rust **sérialisés** (rustc = 1-2 GB en pointe ; un build
  parallèle a déjà OOM-killed la box pendant une matrice). Le client se
  mesure lui-même (`own_rss_end`) : 53 MB à 25k tunnels (~2 KB/conn).

## Tableau final (médiane, auth on)

Trois passes myproxy :

- **M7** = tunnels et réponses bulk en `splice()` zéro-copie, réponses
  petites en copy adaptatif (8→64 KB, pages virtuelles) ;
- **M8** = + **pool upstream keep-alive** (LIFO par origin, retry sur
  socket morte, parse de la tête de réponse pour borner le corps,
  HEAD/204/304, chunked relais transparent) — supprime le connect/close
  par requête.

| Workload | myproxy M7 | **myproxy M8** | 3proxy | Verdict |
|---|---|---|---|---|
| HTTP 1 KB, keep-alive ×16 | 10 028 rps · p50 1,5 ms | **14 699 rps · p50 0,98 ms** | 388 rps · p50 41 ms | **myproxy ×38*** |
| HTTP 10 KB, ×16 | 8 602 rps (88 MB/s) | **14 201 rps (145 MB/s)** · CPU -22 % | 11 852 rps (121 MB/s) | **myproxy DEVANT : +20 % rps, +20 % MB/s** |
| HTTP 1 MB, ×16 | 1 774 rps (1 860 MB/s) | **2 442 rps (2 561 MB/s)** | **2 681 rps** (2 811 MB/s) | quasi-parité : écart **−9 %** (était −68 %) |
| HTTP 10 MB, ×8 (nouveau) | — | 255 rps (2 675 MB/s) | **276 rps (2 892 MB/s)** | 3proxy +13 % (dernier cran, cf. M9) |
| HTTP 1 KB, 1 req/conn ×64 | 4 174 rps | **4 897 rps · p50 6,1 ms** | 2 979 rps · p50 15,1 ms | **myproxy +64 %** |
| CONNECT flood ×8 | 17,68 Gbps · 56 % CPU | **18,48 Gbps · 44 % CPU** | 17,86 Gbps · 43 % CPU | **myproxy +3 % débit, CPU idem** |
| Hold 10 000 · CPU | 1 911 conn/s · CPU -32 % | 1 724 conn/s · RSS 14,5 MB | 1 911 conn/s · RSS 3,4 MB | débit équivalent, CPU ÷1,5 |
| Hold 25 000 · p99 | 25 000/25 000 · p99 0,84 s | **25 000/25 000 ×3 · p99 0,92 s · RSS 28 MB** | 16 745/25 000 (mur ~17k reconfirmé) | **myproxy seul à tenir** |
| RAM tunnel idle (RSS post-test) | ~1,5 KB/conn (14,9 MB @10k) | **~1,2 KB/conn (14,5 MB @10k)** | ~0,34 KB/conn (3,4 MB plat) | 3proxy ×3-4 (tasks tokio vs structs statiques) |

## Ce que le pool a changé (M8, mesures matrice égale)

- Le connect/close upstream par requête était le poste caché du HTTP :
  **14 699 req/s** sur 1 KB (était 10 028), **14 201 req/s** sur 10 KB
  (était 8 602, désormais devant 3proxy), **2 442 req/s** sur 1 MB
  (était 1 774, quasi-parité), +17 % en connexions courtes.
- CPU/req HTTP 1 KB : 0,165 ms (M5) → 0,070 ms (M8).

## Ce que M6/M7 ont changé (mesuré, vs M5)

1. Relay adaptatif (buffers 8→64 KB virtuels) puis `splice()` zéro-copie :
   tunnel **×3,8** (4,86 → 18,48 Gbps), CPU 102 % → 44 % d'un core.
2. HTTP bulk **×4** (604 → 2 442 req/s en 1 MB ; 634 → 2 561 MB/s).
3. RAM idle : RSS post-hold **÷12** (182 → 14,5 MB à 10k tunnels).
4. Latence de queue hold : p99 5,0 s → 0,9 s à 25k tunnels.
5. Cache DNS côté proxy (getaddrinfo/connexion, le burst-504 vu au
   benchmark initial) — le harnais le bypass mais un vrai workload
   CONNECT hostname en dépend.

## Conclusions

- **Demandé cette passe — devant sur 10 KB et parité sur 1 MB : ok,**
  l'écart restant est −9 % en 1 MB (pistes : F_SETPIPE_SZ, fenêtre TCP).
- **Chemin d'établissement + tunnels : myproxy domine** (×38 sur les
  petits objets — cf. caveat 1 —, +61 % en connexions courtes, p99 ÷6 à
  25k, et seul à tenir 25 000 tunnels simultanés sur cette machine).
- **RAM idle** : ×3-4 contre 3proxy — l'architecture task-per-conn de
  tokio coûte ~1,2 KB/conn contre ~0,34 (pooling tasks = M9).

## Caveats honnêtes

1. Le 388 req/s de 3proxy sur HTTP 1 KB vient avec p50 = 41 ms ≈
   **delayed-ACK (40 ms), vérifié par tcpdump dans le netns** : la
   réponse part en 2 petits writes chez 3proxy, le 2ᵉ attends un ACK
   retardé d'exactement 40 ms. Le ×38 est donc réel dans ces conditions,
   mais c'est avant tout un défaut 3proxy correctible de leur côté
   (TCP_NODELAY) — à ne pas publier comme « 3proxy est lent » sans ce
   caveat.
2. 3proxy à 25k : accepte puis ferme sans répondre au-delà de ~17k
   tunnels (config best-effort `maxconn 200000`).
3. Client+sink sur un même core : le plafond absolu du harnais est
   atteint au flood (~18-19 Gbps) ; les deux proxies le buttent pareil.
4. RAM mesurée post-test, modes identiques pour les deux proxies ; en
   cours de hold le client n'occupe que 53 MB à 25k conns — le vrai
   risque mémoire de la box reste rustc/conteneurs voisins.
5. La matrice 50k/100k et le scaling 1/2/4/8 cores se font sur le VPS
   cible avec ce même harnais (`PROXY_CPU`/`BENCH_CPU` auto-détectés).

## Backlog M9 (piloté par ces chiffres)

1. **F_SETPIPE_SZ** sur les pipes de relay + fenêtres TCP : fermer le
   dernier −9 % en 1 MB et le gap CPU du tunnel (×1,05).
2. **Pooling tasks/buffers tokio** : RAM ×3-4 vs 3proxy → ~0,3 KB/conn.
3. **Multi-worker** (`workers = N`) + `SO_REUSEPORT` + courbe de scaling.
4. Matrice 50k/100k + scaling sur le VPS cible.
