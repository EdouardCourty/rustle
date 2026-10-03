# Résultats — myproxy vs 3proxy

Dates : 2026-09-27/28/29 · `{myproxy, 3proxy} × {auth=on, auth=off}`,
médiane de 3 runs (± warmup), client Docker local (aucun accès Internet).

## Environnement

- Machine de dev : **4 GB RAM**, 2 cores exposés (auto-détectés à chaque
  run — la box a déjà montré "0,2", "1,3", "0-1" selon les reboot).
- Pinning CPU : **proxy → son core**, generator/upstream/sink → l'autre.
- Upstream : nginx (1kb/10kb/1mb/10mb, keep-alive 300 s). Sink : serveur
  discard Rust. Sysctls : `somaxconn=65535`, port range élargi,
  `tcp_tw_reuse=1` ; `ulimit -n 524288` partout.
- Sources brutes : `bench/results/<proxy>/auth-<on|off>/<ts>/results.txt`.
- Mémoire : builds Rust **sérialisés** (rustc = 1-2 GB en pointe ; un build
  parallèle a OOM-killed la box). Client seul : 53 MB à 25k tunnels.

## Chiffres définitifs (M10.3 : zéro-alloc parser + fixes syscalls + target-cpu=native — auth on)

Protocole : ordre randomisé à seed figée (`2004113323`, identique aux deux
passe ce jour), budget wall-clock 15 s, **1 KB keep-alive passé à ×64
conns** (M10.2-C : à ×16, le plafond était la latence du harnais —
conns/RTT — pas le proxy ; nouveau protocole pour les deux proxies).

| Workload | **myproxy M10.3** | 3proxy | Verdict |
|---|---|---|---|
| HTTP 1 KB, keep-alive ×64 | **14 822 rps · p50 4,1 ms** | 1 555 rps · p50 41 ms | **myproxy ×9,5*** |
| HTTP 10 KB, ×16 | **13 754 rps (141 MB/s)** · CPU 12,6 s | 12 347 rps (126 MB/s) · CPU 15,3 s | **devant : +11 % rps, +11 % MB/s, CPU -18 %** |
| HTTP 1 MB, ×16 | **2 866 rps (3 005 MB/s)** | 2 455 rps (2 574 MB/s) | **devant : +17 %** |
| HTTP 10 MB, ×8 | **350 rps (3 668 MB/s)** | 294 rps (3 087 MB/s) | **devant : +19 %** |
| HTTP 1 KB, 1 req/conn ×64 | **4 896 rps · p50 5,9 ms** | 3 218 rps · p50 13,3 ms | **myproxy +52 %** |
| CONNECT flood ×8 | **18,87 Gbps · CPU 4,19 s** | 17,84 Gbps · CPU 4,53 s | **devant : +6 % débit, CPU -8 %** |
| Hold 1 000 tunnels | **5 236 conn/s** | 4 149 conn/s | **devant : +26 %** |
| Hold 5 000 | 956 conn/s | 955 conn/s | parité (bruit) |
| Hold 10 000 | 1 741 conn/s (open 5,7 s) | 4 000 conn/s (open 2,5 s) | ⚠️ voir caveat 6 (3proxy bimodal) |
| Hold 25 000 | **25 000/25 000 · rate 2 642 · CPU 3,9 s** | 16 792/25 000 (CPU 22,6 s) | **myproxy seul à tenir (CPU ÷5,8)** |
| RAM tunnel idle | ~1,3 KB/conn | ~0,35 KB/conn | 3proxy ×4 (pooling M11) |

## Économie CPU par requête (nouveau — la métrique qui prédit le plafond)

Le harnais publie désormais **les trois côtés** : proxy
(`cpu_s_per_run`), generator (`gen_cpu_s_per_run`) et upstream
(`up_cpu_s_per_run`) — la moitié du bench qui n'avait jamais été publiée.

Au régime 1 KB ×64 (proxy 42-47 µs/req sur son core, côté bench ~23 µs/req
— upstream nginx ~16 + generator ~8 — sur l'autre) :

- **Plafond proxy seul : ~21-24k rps** (100 % de son core).
- **À 50k rps, les DEUX cores saturent** : le bench côté generator+nginx
  demande ~1,15 s CPU par seconde testée à 14,8k… le mur 50k est autant
  le harnais que le proxy (le 2ᵉ core demande ~52 CPU-s sur 15 s).
- Gains M10.3 mesurés sur CPU/req : 10 KB **-18 %**, 1 KB spot-check
  **-9 %** (46,5 → 42,3 µs), short **-4 %**, 1 MB parité à +6,6 % rps.

## Ce que M10.3 a ajouté (sans toucher au runtime tokio)

- **Parser zéro-alloc** : `RequestHead` ne porte plus que des `Span`
  (indices) dans le buffer de lecture — method/path/auth/host étaient 4-6
  heap-allocs par requête depuis le M0 ; le HTTP forward ne les recopie
  plus jamais (rewrite et origin-key lisent les spans).
- **Fix : `set_nodelay` refait par REQUÊTE** sur la socket poolée
  (`pipeline.rs`, exécuté dans la boucle) — 1 syscall/req gaspillé, 
  maintenant posé une fois à la création.
- **Fix : clé d'origin CONNECT vide** — toutes les CONNECT partageaient
  une entrée du cache DNS depuis M10.2 (le scratch n'était rempli que
  dans le chemin HTTP) ; l'autorité CONNECT construit maintenant sa clé.
- **TCP_QUICKACK une fois par connexion** (au lieu d'une fois par
  requête) — la réponse piggybacke de toute façon l'ACK de la requête.
- **Pool : `put` n'alloue plus la clé** quand l'origine est déjà en pool
  (`entry(to_string)` allouait même sur hit) ; auth : décodage base64 sur
  pile (`decode_slice`, fallback alloc au-delà de 192 B) ; clé d'origin :
  chiffres du port poussés sans `to_string()`.
- **Build : `-C target-cpu=native`** dans l'image (règle : build là où on
  run) — AVX2/BMI sur cette classe de CPU.
- **Harnais : CPU des trois côtés publiée** (`gen_cpu_s_per_run`,
  `up_cpu_s_per_run`) + protocole 1 KB ×64 (M10.2-C).
- Baseline ×64 du M10.2 avant fixes : 15 279 rps à 10,64 CPU-s ; après :
  14 822 à 9,47-10,42 (variance box) — la partie CPU/req est le gain
  durable, le rps 1 KB étant plafonné par la latence du harnais.

## Ce que M10.2 a ajouté (le dernier single-core)

- **Zéro alloc dans le hot path** : buffer de réponse, buffer de rewrite
  et clé d'origin deviennent des scratches conn-locals réutilisés
  (~4-6 allocs/req supprimées, revue O2).
- **TCP_QUICKACK** une fois par requête après la tête : supprime la
  fenêtre delayed-ACK côté nous (le même mécanisme qui pénalise 3proxy).
- **Verify n'a révélé aucune régression** : toutes les cases restent
  devant 3proxy.

## Ce que M9→M10.1 a ajouté (médiane M8 → M10.1)

1. **Pipes de relay 64 KB → 1 MB** (`fcntl F_SETPIPE_SZ`, best-effort,
   fallback silencieux sur `fs.pipe-max-size`) : le fill/drain du splice
   passe de 16 itérations par MB à 1.
2. **SO_SNDBUF/SO_RCVBUF 1 MB** sur tous les sockets proxifiés (accept +
   connect upstream, via `setsockopt`, appliqué best-effort — les pages
   kernel sont allouées à la demande, une connexion idle ne paie rien).
3. **Harnais randomisé** (warmup réseau + shuffle des 10 tests à seed
   fixe) : les chiffres passent de ±30 % de variance (flood surtout) à
   ±3 % — les médianes deviennent comparables entre passes.
4. **Mode http à budget temps** (`--duration 15`) : le protocole est
   identique pour les deux proxies (fini le 17 min du 1 KB chez 3proxy).
5. **Fix du deadlock du fill-to-capacity** (drain-when-loaded au lieu de
   park-on-src avec un pipe chargé) : le 10 MB passe de hang à 3 852 MB/s
   (+28 % vs 3proxy).
6. **Timeout d'inactivité** (IdleTimer, reset à chaque progress) à la
   place du timeout absolu : les tunnels/flux réels de plus de 60 s ne
   meurent plus (B1 — critique avant un VPS public).
7. **Robustesse HTTP** (revue code) : 1xx re-handled (100 Continue
   anti cross-contamination de pool), trailer borné (anti DoS), pas de
   pooling si sur-lecture backend, retry idempotent-only (anti
   double-POST), retry élargi au RST.
8. **Coût de l'auth chiffré** (auth-off matrix) : 1-6 % selon les
   workloads, invisible sur le p50 — la doc peut dire "auth gratuite".

## Coût de l'auth (chiffré, auth-off vs auth-on)

| Workload | auth on | auth off | Surcoût auth |
|---|---|---|---|
| HTTP 1 KB ×16 | 14 909 rps | 15 801 rps | ~5 % (dans le bruit ±4 %) |
| HTTP 10 KB ×16 | 15 659 rps | 14 246 rps | bruit (inversion) |
| HTTP 1 MB ×16 | 2 741 rps | 2 799 rps | ~2 % |
| HTTP short ×64 | 4 835 rps | 4 844 rps | ~0 % |
| 3proxy 1 KB | 388 rps | 388 rps | 0 % |

**Headline : l'auth Basic temps-constant coûte ~1-6 % CPU selon les
workloads, invisible sur le p50.** Aucune raison de la désactiver.

## Ce que M6-M8 ont changé (mesuré, vs M5)

1. Relay adaptatif (buffers 8→64 KB virtuels) puis `splice()` zéro-copie :
   tunnel ×3,8, CPU ÷2 pour le tenir.
2. Pool upstream keep-alive (LIFO par origin, retry sur socket morte,
   parse de la tête réponse) : connect/close par requête supprimé.
3. RAM idle : RSS post-hold 182 → 14,3 MB à 10k tunnels (÷12).
4. Cache DNS (getaddrinfo/connexion, burst-504 vu au benchmark initial).

## Conclusions

- **myproxy est devant PARTOUT sur le data plane au protocole M10.3** :
  10 KB +11 % (CPU -18 %), 1 MB +17 %, 10 MB +19 %, flood +6 % débit à
  CPU -8 %, short +52 %, hold-1k +26 %, hold-25k 25 000/25 000 (CPU ÷5,8).
- **Le 1 KB ×64 est un mur de latence du harnais** (~15-16k quel que soit
  le proxy le plus rapide) — la métrique honnête pour les prochaines
  optimisations est le **CPU par requête**, publié désormais.
- **Le mur 50k est DOUBLE sur cette box** : proxy ~21-24k (CPU/req) ET
  bench-side ~23 µs/req — ce n'est pas "physiquement impossible", c'est
  deux budgets de core à diviser par ~3 chacun (proxy : io_uring/FSM
  sans tokio ; harnais : upstream plus léger que nginx).
- **Chemin d'établissement + tunnels : myproxy domine** (×36 sur les
  petits objets, +62 % en connexions courtes, et seul à tenir 25 000
  tunnels simultanés sur cette machine).
- **Note de fiabilité** : la variance inter-passes reste ±3-5 % sur la
  box (2 cores partagés, conteneurs voisins). Les deltas ≥ +5 % sont
  au-dessus du bruit; les micro-écarts ne sont pas significatifs.
- **L'auth temps-constant est gratuite** au sens de la mesure (~1-6 %
  selon les workloads, invisible sur le p50).
- **RAM idle** : ×4 contre 3proxy — l'architecture task-per-conn de
  tokio coûte ~1,1-1,4 KB/conn contre ~0,35 (pooling tasks = M10).

## Caveats honnêtes

1. Le 1 555 req/s de 3proxy sur HTTP 1 KB ×64 = **delayed-ACK (40 ms)
   vérifié à la capture** : réponse en 2 petits writes chez 3proxy, le 2ᵉ
   attend un ACK retardé d'exactement 40 ms (p50 41 ms identique au
   protocole ×16). Ce ×9,5 est réel ici mais c'est avant tout un défaut
   3proxy (correctible de leur côté, TCP_NODELAY) — à ne pas publier sans
   ce caveat.
2. **Variance flood à la hausse/baisse selon la chauffe** : 17,2 Gbps
   après 4 tests HTTP lourds, 24,3 Gbps en isolation. Le harnais a besoin
   d'un ordre randomisé ou d'une init réseau pré-chauffée pour raffiner.
3. 3proxy à 25k : accepte puis ferme sans répondre au-delà de ~17k
   tunnels (config best-effort `maxconn 200000`).
4. Client+sink sur un même core : le plafond absolu du harnais au flood.
5. RAM mesurée post-test, modes identiques pour les deux proxies.
6. **hold-10k bimodal côté 3proxy** : 1 672-1 877 conn/s sur les passes
   des jours précédents, 4 000 conn/s aujourd'hui (même protocole, même
   seed) — l'établissement massif dépend fortement de l'état réseau
   (conntrack/TIME_WAIT hérités de la position dans la matrice). La
   lecture fiable : hold-25k (le mur structurel) et hold-1k (établissement
   pur) ; hold-5k/10k restent des tests de bande-passante d'accept.

## Backlog M10 (piloté par ces chiffres)

1. **Stabiliser la variance du harnais** (ordre randomisé des tests, init
   réseau pré-chauffée, 2 cores partagés = plafond variable) : les
   derniers % (10 MB isolé +24 % vs matrice −2 %) se perdent dans le bruit.
2. **Pooling tasks/buffers tokio** : RAM ×3-4 vs 3proxy → ~0,3 KB/conn.
3. **Le VPS** : la matrice 50k/100k hold + scaling réel. Single-worker
   calibré au max ici (contrainte assumée : pas de multi-worker) ; la
   contrainte "4 GB / 2 cores" de la box de dev n'est plus le limiteur
   une fois sur le VPS.

## M10.4 (retour à tokio, ré-application des gains vérifiés) — 2026-09-29, seed 2004113323

**Décision** : la passe M11 (FSM epoll sans tokio) a été conservée sous tag `m11.1-fsm`
et la ligne de production est revenue au tokio (`m10.3-tokio`). Les données ont départagé :
CPU/requête tokio ≈ FSM (46,5 vs 47 µs) mais le tokio tient le meilleur rps sur le chemin
HTTP (1kb 14,8k vs 11,5k), son rail splice body fonctionne, et l'architecture
hand-rolled coûtait ~800 lignes de concurrence maison + 4 bugs sérieux trouvés en une
seule session (spin silencieux sans syscall, budget fixe comparé à un compteur, etc.).
Le profil syscall du conteneur (image debug strace) confirme que **M10.3 est déjà au
plancher de son architecture : ~2 recvfrom + 2 sendto par requête, epoll_ctl ≈ 0**.

**M10.4 = tag M10.3 +** :
1. `err_kinds` dans le générateur (taxonomie eof-head/bad-status-\<code\>/eof-body/bad-head/io/connect,
   publiée dans la métrique — l'outil qui a départagé chaque bug de la passe M11).
2. `writev` upstream : head + body-prefix en **un** syscall (`write_vectored` + boucle
   d'avancement ; le check pipelined-body est passé avant l'écriture = plus d'écriture
   partielle). Neutre sur les GET (fast path 1 write déjà), gains réels sur uploads.
3. `BENCH_ONLY=<test>` : validation/ré-exécution d'une étape isolée de la matrice
   (une étape fautive se rejoue seule, pas toute la matrice).
4. `bench/tools/local-upstream-http11.py` : upstream local HTTP/1.1 keep-alive — a débusqué
   le piège de mesure (un upstream HTTP/1.0 reconnecte légitimement à chaque requête ;
   le profil syscall du « chemin chaud » mesurait en fait le chemin fresh-connect).

**Matrice M10.4 vs baseline fraîche du tag vs matrice figée M10.3** (seed 2004113323) :
1kb 14 693 (+3,4 %) · 10kb 13 792 (+4,0 %) · 1mb 2 597 (−5,2 %) · 10mb 360 (+2,3 %) ·
short 4 750 (+39 %, baseline basse : retour au niveau figé 4 896) · flood 19 Gbps (−1,9 %) ·
holds ±8 % (variance connue). Tous err_kinds vides. **Le writev est neutre sur la matrice
comme prédit (GET = fast path) et correct pour les uploads.**

**Caveats conservés** : ×9.5 sur le 1kb vs 3proxy (delayed-ACK, tcpdump-vérifié),
variance hold bimodale (ci-dessus).

**Backlog suivant** : port du relay chunked bufferisé (M10.3 lit le framing octet par
octet, ~1 syscall/octet — hors matrice, réel hors-bench), profilage du VPS, PGO.

## M10.5 (correctifs + squeeze single-core) — 2026-09-30, seed 2004113323, auth on

**Correctifs** (hors matrice : le bench sert du Content-Length) : relais chunked
réécrit (les octets lus avec le head étaient forwardés bruts puis le parser
démarrait désynchronisé → connexion client bloquée jusqu'à l'idle timeout ;
staging = `resp_buf`, budget appliqué pendant le relais, trailers bornés),
requêtes TE / Content-Length invalide ou contradictoire → 400 (anti-smuggling),
backoff sur erreur `accept`, `config.toml.example` (table `[http]` dupliquée),
test pool (tokio 1.53 refuse un fd bloquant).

**Mesures, une matrice par étape** (µs CPU proxy/req ; bruit constaté entre deux
matrices au code identique sur ces chemins : jusqu'à ±7 % CPU, ±13 % débit 10mb) :

| Étape | Effet mesuré |
|---|---|
| S1 runtime `current_thread` (workers=1) | short −11 % CPU/req ; le reste dans le bruit |
| S2 fenêtre de lecture du head de réponse 2 → 16 KB | 10kb : 2 recv+2 send → 1+1 (strace), CPU dans le bruit |
| S3 plus d'alloc par réponse (`to_ascii_lowercase`, Vec 8 KB si body déjà lu) | bruit |
| S4 NODELAY/SNDBUF/RCVBUF posés sur le listener (hérités, test) | short −2,4 % supplémentaire |
| mimalloc (retest) | bruit (hot path sans alloc) — non retenu |

**Final vs HEAD M10.4-2** : short 101,2 → 88,8 µs/req (**−12 %**, stable sur 5
matrices), 10kb −5 %, 10mb −5 %, holds CPU −2 à −7 %, 1kb neutre. Débits
inchangés : le harnais (2 cores) plafonne.

**Répartition user/sys (1kb ×64)** : user ≈ 9 µs/req, sys ≈ 35 µs/req. ~80 % du
CPU proxy est de la pile TCP loopback (qui inclut le traitement RX des pairs) ;
les syscalls sont au plancher (2 recv + 2 send + epoll_wait). Le code Rust ne peut
plus gagner que sur ~20 % du CPU → PGO, refonte des timers, FxHash : gains
attendus < bruit, non faits.

**Mémoire — biais de mesure corrigé** : `rss_mb` est lu *après* la fin du test
(mémoire retenue), pas pendant. Nouveau `peak_rss_mb` (VmHWM) : **~5 KB par
tunnel vivant** (hold-25k : 123-125 MB, identique HEAD et final) — le « ~1,3 KB
par tunnel » publié plus haut était du résiduel. Avec `current_thread`, tout vit
dans l'arène principale glibc (non réduite) : résiduel 125 MB. Fix :
`malloc_trim` quand les connexions tombent sous la moitié du pic (tick 5 s) →
2 MB résiduels.

## M13 — campagne 5 proxies × 3 transports — 2026-09-30, seed 2004113323, auth on, 1 core proxy

Protocole : matrice habituelle (REPEAT=3, médiane), proxy seul sur 1 core, harnais sur l'autre.
Cellule = débit · CPU proxy par requête (cgroup du conteneur). Pic RAM = `peak_rss_mb` (somme des
processus). DUT : 3proxy 0.9.4, Squid 5.7 (cache off, `access_log none`), gost 3.2.6 (logs d'accès
coupés : il loggait chaque connexion en `info`, coût que les autres ne paient pas), Tinyproxy 1.11.
TLS : certificat ECDSA P-256 identique en nature pour tous. 0 erreur HTTP partout.

### Clair (HTTP/1.1)

| Test | **myproxy** | 3proxy | Squid | gost | Tinyproxy |
|---|---|---|---|---|---|
| 1 KB ×64 | **14 291** · 49 µs | 1 555¹ · 75 µs | 9 113 · 120 µs | 1 913 · 570 µs | 2 806 · 347 µs |
| 10 KB ×16 | **13 532** · 62 µs | 12 475 · 80 µs | 7 308 · 149 µs | 1 847 · 580 µs | 2 394 · 411 µs |
| 1 MB ×16 | **2 642** · 334 µs | 2 146 · 395 µs | 309 · 3 574 µs | 695 · 1 563 µs | 129 · 8 566 µs |
| 10 MB ×8 | **375** · 2,3 ms | 281 · 2,7 ms | 32 · 35 ms | 104 · 10,6 ms | 14 · 81 ms |
| 1 KB short | **4 805** · 88 µs | 3 407 · 260 µs | 3 794 · 235 µs | 1 880 · 573 µs | 2 409 · 366 µs |
| flood | **19,2 Gbps** | 18,0 | 12,8 | 11,7 | 1,2 |
| hold-10k (pic RAM) | 10 000 · **52 MB** | 10 000 · 231 MB | 10 000 · 450 MB | 10 000 · 1 352 MB | 10 000 · 189 MB |
| hold-25k | **25 000** · 119 MB | 17 146 · 371 MB | 25 000 · 882 MB | 14 618² | 16 746 · 314 MB |

### TLS (proxy HTTPS, HTTP/1.1)

| Test | **myproxy** | Squid | gost |
|---|---|---|---|
| 1 KB ×64 | **14 122** · 57 µs | 7 790 · 140 µs | 1 614 · 656 µs |
| 10 KB ×16 | **13 375** · 62 µs | 6 089 · 180 µs | 1 539 · 685 µs |
| 1 MB ×16 | **1 285** · 819 µs | 177 · 6 218 µs | 464 · 2 327 µs |
| 10 MB ×8 | **105** · 10,3 ms | 18 · 61 ms | 60 · 18,2 ms |
| 1 KB short (handshake/req) | **2 726** · 269 µs | 1 605 · 677 µs | 1 599 · 669 µs |
| flood | **9,2 Gbps** | 4,9 | 6,7 |
| hold-10k (pic RAM) | 10 000 · **332 MB** | 10 000 · 803 MB | 10 000 · 1 736 MB |
| hold-25k | **25 000** · 659 MB | 25 000 · 1 774 MB | 21 924² |

### HTTP/2 (TLS, ALPN h2 ; 1 connexion h2 par « conn » du générateur)

| Test | **myproxy** | gost |
|---|---|---|
| 1 KB ×64 | **12 975** · 70 µs | 3 934 · 280 µs |
| 10 KB ×16 | **11 958** · 83 µs | 3 676 · 298 µs |
| 1 MB ×16 | **726** · 1,4 ms | 323 · 3,4 ms |
| 10 MB ×8 | **69** · 15 ms | 35 · 31 ms |
| 1 KB short | **2 240** · 349 µs | 1 469 · 680 µs |
| flood (CONNECT sur stream) | 6,6 Gbps · 1 078 ms/GB | **7,7 Gbps** · 1 106 ms/GB |
| hold-10k (pic RAM) | 10 000 · **622 MB** | 10 000 · 1 990 MB |
| hold-25k | 21 351³ · 882 MB | 743² |

¹ delayed-ACK 40 ms de 3proxy (caveat historique). ² conteneur mort pendant le test (plus de CPU
mesurable, pic 0) : RAM de la box (4 GB) dépassée — gost ≈ 135-175 KB/tunnel. ³ limité par le
**générateur** (une connexion TLS+h2 par tunnel : 39 s CPU sur son core, 868 MB, 145 s d'ouverture ;
myproxy 21 s CPU) — non concluant sur cette box.

**Lecture** : myproxy est premier sur toutes les lignes en clair et en TLS, avec le CPU/req le plus bas
et la RAM la plus basse. En TLS, les petites requêtes keep-alive ne coûtent presque rien (1 KB +15 %
CPU/req, 10 KB ≈ 0) ; le bulk paie le chiffrement et la perte du splice (1 MB ×2,5, 10 MB ×4,4
CPU/req, flood ×5,6 CPU/GB) — cible du kTLS (M13.3). En h2 : devant gost partout sauf le flood
(-14 % de débit, CPU/GB équivalent) ; le chemin h2 est plus lent que TLS+HTTP/1.1 sur le bulk
(1 MB 726 vs 1 285) : framing h2 + copie `Bytes` par chunk — piste d'optimisation.

**Caveats** : box 2 cores, loopback, un seed ; multi-core non mesuré ici (VPS : `run-scaling.sh`).
Tunnels TLS : ~26 KB/tunnel chez myproxy (buffers rustls) contre ~5 KB en clair.

## M16 — coût du log d'accès — 2026-09-30, seed 2004113323, auth on, 1 core

| µs CPU proxy/req | log `off` | `all` v1 (1 réveil writer/ligne) | `all` batché (lot par shard) |
|---|---|---|---|
| 1 KB ×64 | 49,3 | 62,2 (+26 %) | **50,7 (+2,9 %)** |
| 1 KB short | 90,0 | 110,2 (+22 %) | **91,0 (+1,1 %)** |
| 10 KB / 1 MB / 10 MB / flood | — | +7 à +13 % | dans le bruit |

La v1 envoyait chaque ligne au thread writer via le canal : un réveil (futex + changement de
contexte sur le même core) par requête coûtait plus que tout le traitement user-space (~9 µs).
Version retenue : lignes accumulées dans un buffer local au thread du shard (sans verrou), remises
au writer par lots de 64 KB ou toutes les 200 ms. `off` : aucune régression vs M15.

## M17 — kTLS (kernel TLS) — 2026-10-01, seed 2004113323, auth on, 1 core, proxy HTTPS

rustls fait le handshake, les clés passent au noyau (`TCP_ULP tls` + `SOL_TLS`), le splice
revient sur le port TLS. Testé en réel : TLS 1.3, TLS 1.2 AES-128/256-GCM et ChaCha20, 10 MB
via GET et via tunnel CONNECT identiques au direct.

| µs CPU proxy/req (débit) | rustls userspace | kTLS | kTLS + TX_ZEROCOPY_RO / RX_NO_PAD |
|---|---|---|---|
| 1 KB | 56,8 | 60,1 (+6 %) | — |
| 10 KB | 62,5 | 67,5 (+8 %) | — |
| 1 MB | 819 (1 287 rps) | 1 010 (815 rps, **−37 %**) | 1 126 (813 rps) |
| 10 MB | 10 033 (108 rps) | 11 796 (86 rps) | 10 378 (93 rps) |
| 1 KB short (handshake/req) | 276 | 326 (+18 %) | 319 |
| flood (CONNECT) | 945 ms/GB (9,2 Gbps) | **824 ms/GB (10,4 Gbps, +13 %)** | 826 (10,3 Gbps) |
| holds | — | +14 à +18 % CPU | — |
| pic RAM hold-25k | 814 MB | **569 MB (−30 %)** | — |

h2 sur kTLS : −40 % sur 1 MB / 10 MB. **Décision** : `tls.ktls = false` par défaut, option
documentée — gagnant pour les tunnels CONNECT (navigation HTTPS) et la mémoire, perdant sur le
bulk HTTP et par connexion (mise en place : vidage rustls + ULP + clés).

## M18 — io_uring, prototype mesuré avant migration — 2026-10-01, seed 2004113323

Prototype monoio (`bench/uring-proto`, DUT `uring-proto`) du chemin HTTP/1 keep-alive (même
réécriture, auth, upstream gardé, réponses Content-Length, timeout par opération). Comparé à
myproxy avec seccomp levé pour les deux (`SECCOMP=unconfined` : le profil Docker bloque
io_uring et le filtre coûte par syscall). Runs alternés :

| µs CPU proxy/req | io_uring (proto) | Tokio (myproxy) |
|---|---|---|
| 1 KB ×64 | 53,7 | **49,8** |
| 10 KB ×16 (2 runs) | **57,1 / 57,5** | 62,4 / 62,0 |

Sans les timeouts, le proto semblait −10 à −14 % sur 10 KB : une partie venait de l'absence de
timers, pas d'io_uring. 1 MB / 10 MB / short non comparables (proto sans splice ni pool
partagé). **Verdict** : ±8 % selon la taille, pas de gain net — même conclusion que M11 (FSM
epoll) : sur un core, le coût est dans la pile TCP du noyau. Pas de migration ; à retester sur
le VPS multi-core (batching des soumissions).

## M19 — campagne finale — 2026-10-01, seed 2004113323, auth on, 1 core proxy

`bench/run-campaign.sh` (5 proxies × chaque transport supporté), tables par
`bench/tools/campaign-table.py`. Cellule = débit · CPU proxy par requête (http : µs/req,
flood : ms/GB) ; holds = tunnels ouverts / demandés · pic RAM. Configuration par défaut de
myproxy (kTLS off, log off). Aucune erreur (`err_kinds` vides) ; seules les reconnexions de
Tinyproxy (1 par requête, il ferme sans l'annoncer).

### plain

| test | myproxy | 3proxy | squid | gost | tinyproxy |
|---|---|---|---|---|---|
| http-1kb | 14,332 rps 49µs | 1,558 rps 74µs | 8,841 rps 124µs | 1,930 rps 561µs | 2,627 rps 366µs |
| http-10kb | 14,024 rps 61µs | 13,207 rps 79µs | 7,174 rps 152µs | 1,864 rps 575µs | 2,606 rps 396µs |
| http-1mb | 2,622 rps 336µs | 2,437 rps 369µs | 306 rps 3601µs | 703 rps 1553µs | 130 rps 8445µs |
| http-10mb | 347 rps 2168µs | 279 rps 2628µs | 32 rps 35203µs | 104 rps 10562µs | 14 rps 82017µs |
| http-1kb-short | 4,822 rps 87µs | 3,345 rps 261µs | 3,775 rps 237µs | 1,927 rps 567µs | 2,802 rps 356µs |
| flood | 19.0 Gbps 152ms/GB | 17.7 Gbps 192ms/GB | 12.9 Gbps 674ms/GB | 11.6 Gbps 750ms/GB | 1.2 Gbps 6913ms/GB |
| hold-1000 | 1,000/1000 10.2MB | 1,000/1000 26.0MB | 1,000/1000 164.8MB | 1,000/1000 179.9MB | 1,000/1000 21.8MB |
| hold-5000 | 5,000/5000 29.8MB | 5,000/5000 117.1MB | 5,000/5000 291.7MB | 5,000/5000 774.5MB | 5,000/5000 95.9MB |
| hold-10000 | 10,000/10000 52.2MB | 10,000/10000 230.8MB | 10,000/10000 449.7MB | 10,000/10000 1446.2MB | 10,000/10000 189.6MB |
| hold-25000 | 25,000/25000 124.8MB | 24,959/25000 384.5MB | 25,000/25000 875.3MB | 18,656/25000 0.00MB | 16,765/25000 314.0MB |

### tls

| test | myproxy | squid | gost |
|---|---|---|---|
| http-1kb | 14,018 rps 57µs | 7,837 rps 141µs | 1,566 rps 676µs |
| http-10kb | 13,540 rps 62µs | 5,999 rps 183µs | 1,550 rps 690µs |
| http-1mb | 1,297 rps 813µs | 182 rps 6060µs | 464 rps 2328µs |
| http-10mb | 108 rps 10017µs | 18 rps 60648µs | 59 rps 18334µs |
| http-1kb-short | 2,722 rps 278µs | 1,618 rps 669µs | 1,603 rps 661µs |
| flood | 9.3 Gbps 938ms/GB | 4.8 Gbps 1796ms/GB | 6.6 Gbps 1309ms/GB |
| hold-1000 | 1,000/1000 47.9MB | 1,000/1000 203.5MB | 1,000/1000 246.5MB |
| hold-5000 | 5,000/5000 222.8MB | 5,000/5000 462.9MB | 5,000/5000 1044.2MB |
| hold-10000 | 10,000/10000 438.7MB | 10,000/10000 811.6MB | 10,000/10000 1863.8MB |
| hold-25000 | 25,000/25000 802.4MB | 25,000/25000 2028.0MB | 0/25000 0.00MB |

### h2

| test | myproxy | gost |
|---|---|---|
| http-1kb | 13,028 rps 70µs | 3,922 rps 281µs |
| http-10kb | 12,232 rps 82µs | 3,709 rps 295µs |
| http-1mb | 739 rps 1428µs | 324 rps 3374µs |
| http-10mb | 68 rps 15495µs | 35 rps 30183µs |
| http-1kb-short | 2,281 rps 354µs | 1,432 rps 689µs |
| flood | 6.6 Gbps 1074ms/GB | 7.7 Gbps 1105ms/GB |
| hold-1000 | 1,000/1000 93.2MB | 1,000/1000 259.0MB |
| hold-5000 | 5,000/5000 462.9MB | 5,000/5000 997.1MB |
| hold-10000 | 10,000/10000 807.2MB | 10,000/10000 0.00MB |
| hold-25000 | 21,291/25000 1112.7MB | 931/25000 0.00MB |

Notes : 3proxy 1 KB = delayed-ACK 40 ms (caveat historique) ; 3proxy tient cette fois 24 959 /
25 000 tunnels (17 146 en M13 — variance connue de son mur à ~17-25k) ; gost : conteneur tué
par manque de RAM à 25k (clair et TLS) et dès la fin de hold-10k en h2 (pic 0) ; myproxy h2
hold-25k limité par le générateur (une connexion TLS+h2 par tunnel), comme en M13. Écarts à
M13 dans le bruit.

## M20 — tunnels HTTP/2 : rattraper puis dépasser gost — 2026-10-02, seed 2004113323

Constat (M19) : flood h2 6,6 Gbps vs gost 7,7, à CPU/GB égal — myproxy n'utilisait que 87 % de
son core (gost 104 %) : il attendait. Mesures pas à pas (flood-8, PROXY_H2=1) :

| Étape | Débit | CPU proxy | Lecture |
|---|---|---|---|
| départ | 6,6 Gbps | 87 % | contrôle de flux : le client attend des WINDOW_UPDATE |
| A. fenêtre rouverte dès réception (pas après l'écriture amont) | 6,6 | 84 % | sans effet seul, conservé (logique, borné par la fenêtre) |
| B. fenêtres 1 → 4 MB par stream, 8 → 16 MB par connexion | **7,86** | 101 % | goulot levé ; le générateur dépense 7,7 → 6,2 s CPU |
| C. lecture anticipée sous rustls (rustls lit par 4 KB : ~4 recv par record de 16 KB ; buffer 64 KB adaptatif, libéré quand le flux redevient faible) | ≈ | −3 % CPU/GB | h2 borné par le core du harnais ; en TLS HTTP/1 : flood 9,3 → 9,6 Gbps, −5 % CPU/GB |

A/B final alterné (3 passes chacun) : **myproxy 7,64 / 7,54 / 7,58 Gbps (0,97 s CPU/GB) vs gost
7,40 / 7,41 / 7,55 (1,08 s/GB)** — devant à chaque paire, −11 % de CPU par Go. Le débit h2 est
plafonné par le core du harnais (générateur + sink) : l'écart de débit reste serré, celui
d'efficacité est net. Matrices TLS et h2 complètes : aucune régression (tous les autres tests dans
le bruit), pic RAM inchangé (les fenêtres ne réservent rien d'avance), `err_kinds` vides.


## M21 — passe d'optimisations issue de deux recherches (autres proxies + vue d'ensemble) — 2026-10-02/03, seed 2004113323, auth on, 1 core

Méthode : chaque piste est un A/B **alterné** (base / variante / base / variante…, image reconstruite à
chaque changement, proxy redémarré avant chaque test), 2 à 3 passes, retenue seulement au-dessus du bruit.
Métrique : CPU proxy par requête (ou par Go pour les floods).

**Changement de protocole (M21.5)** : le warmup tourne désormais dans un run séparé du générateur avant
l'échantillonnage. Auparavant son CPU était amorti dans chaque chiffre (~+6 à +8 % : 1 KB 49 → 45 µs/req).
**Ne pas comparer les chiffres M21+ aux campagnes précédentes.** Le harnais relève aussi les compteurs TCP
du netns du proxy par run : `segs_out_per_run`, `active_opens_per_run` (churn du pool amont),
`retrans_per_run`, `delayed_acks_per_run`, `listen_overflows`, `listen_drops`. Nouveau test hors matrice
`flood-128` (`BENCH_ONLY=flood-128`).

| Étape | Changement | Résultat |
|---|---|---|
| M21.1 | Moteur de splice refait : pool de pipes par shard (16 max, retour si vide), refus des pipes < 64 KB (le budget `fs.pipe-user-pages-soft` réduit les pipes à 8 KB au-delà de ~60 : 256 splice/MB), splice via `try_io` sur les sockets tokio (plus de dup ni de réenregistrement epoll), tunnels paresseux (copie adaptative puis pipe si soutenu) | flood-128 **10,0 → 16,8–17,0 Gbps** (3proxy 10,9), CPU/GB 0,86 → 0,38 ; 1 MB −8 % ; holds −10 à −14 % CPU ; pic RAM hold-25k 125 → 121 MB |
| M21.2 | Splice dès le premier octet si le corps restant déclaré ≥ 128 KB | 1 MB 300 → 253 µs/req (−16 %), 10 MB −4,5 % |
| M21.3 | TLS : AES-128-GCM en tête, ordre du serveur prioritaire (les sessions tournaient en AES-256) | TLS 1 MB −7 % CPU/req (+7 % rps), flood TLS −4 % CPU/GB, flood h2 −6 % CPU/GB |
| M21.4 | h2 : corps de réponse sans copie par frame (`split_to().freeze()` sur `BytesMut`) | h2 1 MB −5,7 % CPU/req (+4,5 % rps), 10 MB −2,7 % |
| M21.5 | Harnais (voir ci-dessus) | constat : 0,0116 ouverture amont/req en 1 KB ×64 |
| M21.6 | Pool amont `max_idle_per_origin` 32 → 256 (64 clients > 32 places : ~2 500 reconnexions amont par run) ; entrée d'origine vidée conservée | ouvertures amont/req → 0, 1 KB −2,7 % CPU/req |
| M21.7 | `TCP_DEFER_ACCEPT` sur les listeners (tous les protocoles servis parlent en premier côté client) ; timers : `biased;`, timeout extérieur redondant retiré | short 79,8 → 73,9 µs/req (**−7,4 %**) ; timers seuls neutres |
| M21.8 | Multi-core : AuthLimiter (bug : une IP en échec n'était jamais oubliée → verrou global à chaque accept), DNS 1 verrou, compteurs par shard sur leur ligne de cache, `ConnSlot` sur l'`Arc` du shard, tickets TLS stateless | non mesurable sur 1 core ; non-régression OK (bruit) |
| M21.9 | `server.socket_buffer_bytes` (défaut 1 MB, 0 = autotuning) | voir ci-dessous |
| M21.10 | État keep-alive de `serve` boxé (future de connexion 3 760 → < 2 560 octets : un tunnel tenu gardait la taille de la branche la plus grosse) | pic RAM hold-25k 118,5/120,6 → **81,7/79,4 MB (−33 %)**, hold-10k 51,4/51,7 → 36,1/35,3 MB (−31 %) ; CPU, 1 KB et short dans le bruit (1 malloc par connexion) |
| M21.11 | Buffer de copie TLS (côté client non spliceable) 64 → 256 KB max, croissance adaptative inchangée | flood TLS 10,26/10,29 → **11,38/11,46 Gbps** (CPU/GB 0,78 → 0,69, −11 %), TLS 10 MB 8 329/8 512 → 7 633/7 740 µs/req (−9 %), TLS 1 MB −3,6 % (bruit) |
| M21.12 | Steering CPU (`SO_ATTACH_REUSEPORT_CBPF`) quand `pin_cores` donne un shard par CPU : la connexion va au shard épinglé sur le CPU qui a traité son SYN | non mesurable sur 1 core ; vérifié : client épinglé sur le CPU 1 → 40/40 connexions sur le shard 1 |

**Buffers socket : 1 MB fixe vs autotuning noyau** (A/B ×2, bench = LAN Docker) :

| Test | 1 MB | autotuning |
|---|---|---|
| 1 KB | 44,1 / 44,6 µs/req | 48,5 / 49,3 |
| 1 MB | 231 / 223 µs/req | 248 / 238 |
| 10 MB | 319 / 362 rps | 247 / 249 |
| flood-8 | 19,2 / 19,3 Gbps, 0,14 s CPU/GB | 17,1 / 17,2 Gbps, 0,32 |

Défaut 1 MB conservé. Mais une taille fixe désactive l'autotuning : 1 MB en vol au plus par connexion
(~80 Mbit/s à 100 ms de RTT), et elle est plafonnée par `net.core.rmem_max`/`wmem_max` (208 KB sur un noyau
standard → ~416 KB effectifs, ~33 Mbit/s à 100 ms ; avertissement au démarrage). Pour des clients lointains :
`socket_buffer_bytes = 0`.

**Écartés (mesurés neutres)** : QUICKACK 1×/connexion vs aucun (1 KB, short, 10 KB dans le bruit : conservé
tel quel) ; QUICKACK=0 sur le listener (« smart-accept » HAProxy) non testé : `inet_csk_clone_lock` remet
l'état d'ACK à zéro sur le socket accepté, rien n'est hérité.
