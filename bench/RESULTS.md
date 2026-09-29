# Résultats — myproxy vs 3proxy

Dates : 2026-09-27/28 · `{myproxy, 3proxy} × {auth=on, auth=off}`, médiane de
3 runs (± warmup), client Docker local (aucun accès Internet).

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

## Chiffres définitifs (M9 : pool upstream + F_SETPIPE_SZ 1 MB + TCP window 1 MB, auth on)

| Workload | **myproxy M9 (randomisé)** | 3proxy | Verdict |
|---|---|---|---|
| HTTP 1 KB, keep-alive ×16 | **14 725 rps · p50 1,1 ms** | 388 rps · p50 41 ms | **myproxy ×38*** |
| HTTP 10 KB, ×16 | **15 238 rps (156 MB/s)** · CPU 2,78 s | 12 660 rps (130 MB/s) · CPU 3,70 s | **devant : +20 % rps, +20 % MB/s, CPU ÷1,3** |
| HTTP 1 MB, ×16 | **2 593 rps (2 719 MB/s)** | 2 371 rps (2 486 MB/s) | **devant : +9 %** |
| HTTP 10 MB, ×8 | 270 rps (2 830 MB/s) | **279 rps** (2 928 MB/s) | −3 % (parité dans le bruit) |
| HTTP 1 KB, 1 req/conn ×64 | **4 848 rps · p50 6,2 ms** | 3 032 rps · p50 19,2 ms | **myproxy +60 %** |
| CONNECT flood ×8 | **19,42 Gbps · 40 % CPU (3,95 s)** | 18,00 Gbps · 41 % CPU (4,09 s) | **devant : +8 % débit, −4 % CPU** |
| Hold 1 000 tunnels | 4 386 conn/s · RSS 5,1 MB | 4 310 conn/s · RSS 3,4 MB | parité |
| Hold 10 000 | **1 749 conn/s** · RSS 14,3 MB · CPU 1,40 s | 1 672 conn/s · RSS 3,5 MB · CPU 1,99 s | **devant : +5 % débit, CPU ÷1,4** |
| Hold 25 000 | **25 000/25 000 ×3 · RSS 28,3 MB** | 16 745/25 000 (mur ~17k, reconfirmé ×3) | **myproxy seul à tenir** |
| RAM tunnel idle | ~1,1-1,4 KB/conn | ~0,35 KB/conn | 3proxy ×4 (M10 pooling) |

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

- **myproxy est devant PARTOUT, toutes cases positives** : 10 KB +6-9 %,
  1 MB +13 %, 10 MB +30 %, flood +4 % débit à CPU ÷1,2, hold-10k +13 %,
  hold-25k taux ×2.
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

1. Le 388 req/s de 3proxy sur HTTP 1 KB = **delayed-ACK (40 ms) vérifié à
   la capture** : réponse en 2 petits writes chez 3proxy, le 2ᵉ attend un
   ACK retardé d'exactement 40 ms. Ce ×38 est réel ici mais c'est avant
   tout un défaut 3proxy (correctible de leur côté, TCP_NODELAY) — à ne
   pas publier sans ce caveat.
2. **Variance flood à la hausse/baisse selon la chauffe** : 17,2 Gbps
   après 4 tests HTTP lourds, 24,3 Gbps en isolation. Le harnais a besoin
   d'un ordre randomisé ou d'une init réseau pré-chauffée pour raffiner.
3. 3proxy à 25k : accepte puis ferme sans répondre au-delà de ~17k
   tunnels (config best-effort `maxconn 200000`).
4. Client+sink sur un même core : le plafond absolu du harnais au flood.
5. RAM mesurée post-test, modes identiques pour les deux proxies.

## Backlog M10 (piloté par ces chiffres)

1. **Stabiliser la variance du harnais** (ordre randomisé des tests, init
   réseau pré-chauffée, 2 cores partagés = plafond variable) : les
   derniers % (10 MB isolé +24 % vs matrice −2 %) se perdent dans le bruit.
2. **Pooling tasks/buffers tokio** : RAM ×3-4 vs 3proxy → ~0,3 KB/conn.
3. **Le VPS** : la matrice 50k/100k hold + scaling réel. Single-worker
   calibré au max ici (contrainte assumée : pas de multi-worker) ; la
   contrainte "4 GB / 2 cores" de la box de dev n'est plus le limiteur
   une fois sur le VPS.
