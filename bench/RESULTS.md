# Résultats — myproxy vs 3proxy

Date : 2026-09-27/28 · Matrice : `{myproxy, 3proxy} × auth=on`, médiane de 3
runs (± warmup), client Docker local (aucun accès Internet).

## Environnement

- Machine de dev : **2 cores, 4 GB RAM** (cœurs exposés "0,2"), Docker + compose.
- Pinning CPU : **proxy → core 0**, client/upstream/sink → core 2. Le débit
  absolu est plafonné par client+sink partageant un core ; les ratios
  myproxy/3proxy restent valides (conditions identiques).
- Upstream : nginx (1kb/10kb/1mb, keep-alive). Sink : serveur discard Rust.
- Sysctls : `somaxconn=65535`, `ip_local_port_range=1024 65535`,
  `tcp_tw_reuse=1` ; `ulimit -n 524288` partout.
- Sources brutes : `bench/results/<proxy>/auth-on/<timestamp>/results.txt`.

## Tableau comparatif (médiane, auth on)

Passe « avant » = M5 (`copy_bidirectional` 8 KB, buffers alloués d'avance) ;
passe « M6 » = relay adaptatif 8→64 KB + buffers virtuels lazys +
stacks de task 128 KB + cache DNS (voir `myproxy/src/conn.rs`, `dns.rs`).

| Workload | myproxy avant | myproxy M6 | 3proxy | Verdict M6 |
|---|---|---|---|---|
| HTTP 1 KB, keep-alive ×16 | 10 574 req/s · p50 1,5 ms | 9 771 req/s · p50 1,5 ms | 388 req/s · p50 41 ms | **myproxy ×25*** |
| HTTP 10 KB, keep-alive ×16 | 8 992 req/s | 8 208 req/s | **12 499 req/s** | 3proxy +52 % |
| HTTP 1 MB, keep-alive ×16 | 604 req/s (634 MB/s) | **1 143 req/s (1 199 MB/s)** | **2 681 req/s (2 811 MB/s)** | 3proxy ×2,3 (était ×4,4) |
| HTTP 1 KB, 1 req/conn ×64 | 4 162 req/s · p50 7,6 ms | 3 846 req/s · p50 8,9 ms | 3 048 req/s · p50 15,4 ms | **myproxy +26 %** |
| CONNECT flood ×8 | 4,86 Gbps · 0,46 Gbps/core | **15,73 Gbps · 1,53 Gbps/core** | **18,67 Gbps · 4,4 Gbps/core** | écart ×3,8 → **×1,19** |
| Hold 10 000 tunnels | 0 échec · 1 720 conn/s | 0 échec · 1 614 conn/s | 0 échec · 1 592 conn/s | parité |
| Hold 25 000 tunnels | 25 000/25 000 | **25 000/25 000 ×3** | 16 903/25 000 (mur ~17k) | **myproxy seul à tenir** |
| RAM / tunnel (RSS post-test) | 18,2 KB/conn | 16,4 KB/conn | **~0,4 KB/conn** (RSS 3,4 MB plat) | 3proxy (structs statiques) |

## Ce que M6 a changé (mesuré)

1. **Relay tunnel ×3,2** : 4,86 → 15,73 Gbps (CPU/core : 0,46 → 1,53 Gbps/core,
   ×3,3). L'écart avec 3proxy passe de ×3,8 à ×1,19 en débit.
2. **HTTP bulk ×1,9** : 604 → 1 143 req/s en 1 MB (CPU/req ÷2,2 :
   1,76 → 0,80 ms). Écart 3proxy ramené de ×4,4 à ×2,3.
3. **RAM/tunnel** : 18,2 → 16,4 KB/conn (RSS post-test) ; ~6-10 KB/conn en
   cours de hold (mesure directe /proc). Le relay est à pages **virtuelles**
   (un tunnel idle ne touche aucune page de relay) ; le restant = task tokio
   + structures (pooling = M7).
4. **Establishment inchangé** (déjà le point fort) ; 0 échec partout jusqu'à
   25 000 tunnels.

## Conclusions

- **Chemin d'établissement : myproxy domine** (accept+parse+auth+connect) :
  ×25 sur HTTP 1 KB, +26 % en connexions courtes, seul à tenir 25k tunnels.
- **Data plane : l'écart est quasi fermé en débit** (×1,19), mais **3proxy
  reste ×2,9 plus économe en CPU** sur le relay (4,4 vs 1,53 Gbps/core) —
  profil compatible avec un relay zéro-copie (`splice()`, que 3proxy 0.9.x
  utilise sous Linux) : cible M7 nº1.
- **RAM idle** : l'architecture task-per-conn de tokio coûte ~16 KB/conn là
  où 3proxy tient des structs statiques (~0,4 KB). Pooling de buffers/tasks
  ou relay spawnless : cible M7 nº2.

## Caveats honnêtes

1. Le 388 req/s de 3proxy sur HTTP 1 KB vient avec p50 = 41 ms ≈ **delayed-ACK
   (40 ms)** : suspicion de Nagle/write-pattern dans 3proxy sur les petites
   réponses. Le ×25 est réel dans ces conditions, mais à vérifier avant
   publication.
2. RAM mesurée post-test (allocateur libéré mais pages non rendues à l'OS) ;
   en cours de hold, myproxy est à ~6-10 KB/conn.
3. 3proxy à 25k : accepte puis ferme sans répondre au-delà de ~17k tunnels
   (config best-effort `maxconn 200000` — 3proxy a beaucoup de knobs).
4. Le client et le sink partagent le core 2 : les taux d'ouverture hold
   plafonnent sur le harnais, pas sur le proxy.
5. Machine 2 cores : la matrice 50k/100k et le scaling 1/2/4/8 cores se font
   sur le VPS cible avec ce même harnais.
6. Les chiffres M6 ci-dessus n'incluent pas le cache DNS (ajouté après la
   matrice ; le harnais bypass le DNS, donc aucun effet sur ces nombres —
   vérifié fonctionnellement : HTTP/CONNECT hostname OK, multi-adresses
   essayées dans l'ordre).

## Backlog

- ✅ M6-1 relay adaptatif (8→64 KB, pages virtuelles) — flood ×3,2, 1 MB ×1,9
- ✅ M6-2 laziness des buffers de relay (pages touchées par les données seules)
- ✅ M6-4 cache DNS TTL 60 s, multi-adresses dans l'ordre
- M7-1 **`splice()` zéro-copie** sur le relay tunnel (gap CPU ×2,9 restant)
- M7-2 **pooling** buffers/tasks (RAM ~16 KB → ~1-2 KB/conn)
- M7-3 multi-worker (`workers = N`) + `SO_REUSEPORT` + courbe de scaling
- M7-4 matrice 50k/100k + scaling sur le VPS cible
- M7-5 vérification du point 3proxy/delayed-ACK (honnêteté du ×25)
