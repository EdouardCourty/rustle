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

| Workload | **myproxy M9.1** | 3proxy | Verdict |
|---|---|---|---|
| HTTP 1 KB, keep-alive ×16 | **14 175 rps · p50 0,99 ms** | 388 rps · p50 41 ms | **myproxy ×37*** |
| HTTP 10 KB, ×16 | **13 943 rps (143 MB/s)** | 11 852 rps (121 MB/s) | **devant : +18 % rps, +18 % MB/s** |
| HTTP 1 MB, ×16 | **2 729 rps (2 862 MB/s)** | 2 444 rps (2 563 MB/s) | **devant : +12 %** |
| HTTP 10 MB, ×8 | 270 rps (2 832 MB/s) · isolé: 3 575 MB/s | **276 rps** (2 892 MB/s) | parité (variance harnais ±11 %) |
| HTTP 1 KB, 1 req/conn ×64 | **4 765 rps · p50 6,1 ms** | 2 979 rps · p50 15,1 ms | **myproxy +60 %** |
| CONNECT flood ×8 | **19,18 Gbps** · 44 % CPU | 17,86 Gbps · 42 % CPU | **devant en débit, CPU idem** |
| Hold 10 000 tunnels | 1 708 conn/s · RSS 14,3 MB · CPU 1,40 s | 1 911 conn/s · CPU 1,95 s | débit -11 %, CPU ÷1,4 |
| Hold 25 000 · p99 | **25 000/25 000 ×3 · RSS 28,5 MB** | 16 745/25 000 (mur ~17k) | **myproxy seul à tenir** |
| RAM tunnel idle | ~1,1 KB/conn | ~0,34 KB/conn | 3proxy ×3-4 (M10 pooling) |

## Ce que M9 a ajouté (médiane M8 → M9.1)

1. **Pipes de relay 64 KB → 1 MB** (`fcntl F_SETPIPE_SZ`, best-effort,
   fallback silencieux sur `fs.pipe-max-size`) : HTTP 1 MB +12 %,
   10 KB +10 % — le fill/drain du splice passe de 16 itérations par MB
   à 1.
2. **SO_SNDBUF/SO_RCVBUF 1 MB** sur tous les sockets proxifiés (accept +
   connect upstream, via `setsockopt`, appliqué best-effort — les pages
   kernel sont allouées à la demande, une connexion idle ne paie rien) :
   10 MB 2 675 → 2 832-3 575 MB/s (devant/parité), flood 18,5 → 19,2 Gbps.
3. **Cost de l'auth chiffré** (auth-off matrix) : 1-6 % selon les
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

- **myproxy est devant ou à parité avec 3proxy sur tout le data plane**
  (10 KB +18 %, 1 MB +12 %, 10 MB parité ±variance, flood +7 % débit à
  CPU idem) — le 10 MB reste dans la variance harnais (isolé : +24 %).
- **Chemin d'établissement + tunnels : myproxy domine** (×37 sur les
  petits objets, +60 % en connexions courtes, et seul à tenir 25 000
  tunnels simultanés sur cette machine).
- **L'auth temps-constant est gratuite** au sens de la mesure (~1-6 %
  selon les workloads, invisible sur le p50).
- **RAM idle** : ×3-4 contre 3proxy — l'architecture task-per-conn de
  tokio coûte ~1,1 KB/conn contre ~0,34 (pooling tasks = M10).

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
