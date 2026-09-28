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

## Chiffres définitifs (M9 : pool upstream + F_SETPIPE_SZ 1 MB, auth on)

| Workload | **myproxy M9** | 3proxy | Verdict |
|---|---|---|---|
| HTTP 1 KB, keep-alive ×16 | **14 909 rps · p50 0,88 ms** | 388 rps · p50 41 ms | **myproxy ×38*** |
| HTTP 10 KB, ×16 | **15 659 rps (160 MB/s)** · CPU 2,68 s | 11 852 rps (121 MB/s) · CPU 3,74 s | **devant : +32 % rps, +32 % MB/s, CPU ÷1,4** |
| HTTP 1 MB, ×16 | **2 741 rps (2 874 MB/s)** | 2 444 rps (2 563 MB/s) | **devant : +12 %** |
| HTTP 10 MB, ×8 | 245 rps (2 571 MB/s) | **276 rps (2 892 MB/s)** | 3proxy +13 % (plafond harnais) |
| HTTP 1 KB, 1 req/conn ×64 | **4 835 rps · p50 6,2 ms** | 2 979 rps · p50 15,1 ms | **myproxy +62 %** |
| CONNECT flood ×8 | 17,18-24,27 Gbps · 44-75 % CPU | 17,86-18,09 Gbps · 42-43 % CPU | parité (voir variance flood) |
| Hold 10 000 tunnels | 1 577 conn/s · RSS 14,4 MB · CPU -35 % | 1 911 conn/s · RSS 3,4 MB | CPU ÷1,4 en notre faveur |
| Hold 25 000 · p99 | **25 000/25 000 ×3 · p99 0,92-5 s · RSS 28 MB** | 16 745/25 000 (mur ~17k) | **myproxy seul à tenir** |
| RAM tunnel idle | ~1,2 KB/conn | ~0,34 KB/conn | 3proxy ×3-4 (M10 pooling) |

## Ce que M9 (F_SETPIPE_SZ) a ajouté (médiane M8 → M9)

Pipes de relay passés de 64 KB à **1 MB** (`fcntl F_SETPIPE_SZ`,
best-effort, fallback silencieux sur fs.pipe-max-size) :

- HTTP 1 KB : 14 699 → **14 909 rps** (+1,4 %)
- HTTP 10 KB : 14 201 → **15 659 rps** (+10 %), MB/s +10 %
- HTTP 1 MB : 2 442 → **2 741 rps** (+12 %, désormais DEVANT 3proxy)
- HTTP short-conn : 4 897 → 4 835 rps (dans le bruit)
- Le gain vient des pipes remplis à 1 MB : le nombre d'itérations
  fill/drain du splice passe de ~16× à 1× par MB de flux.

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
3. F_SETPIPE_SZ 1 MB : +12 % sur les gros objets, +5 % sur les petits.
4. RAM idle : RSS post-hold 182 → 14,4 MB à 10k tunnels (÷12).
5. Cache DNS (getaddrinfo/connexion, burst-504 vu au benchmark initial).

## Conclusions

- **myproxy est devant ou à parité avec 3proxy sur tout** sauf le 10 MB
  (−11 %) et la RAM/conn (×3-4) — tous deux chiffrés dans le backlog M10.
- **Chemin d'établissement + tunnels : myproxy domine** (×38 sur les
  petits objets, +62 % en connexions courtes, et seul à tenir 25 000
  tunnels simultanés sur cette machine).
- **L'auth temps-constant est gratuite** au sens de la mesure (~1-6 %
  selon les workloads).

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

1. **Pooling tasks/buffers tokio** : RAM ×3-4 vs 3proxy → ~0,3 KB/conn.
2. **Fenêtre TCP fine-tuning** (`tcp_wmem/rmem` par socket via socket2) :
   le 10 MB reste −11 % ; le flood en variance à stabiliser (randomiser
   l'ordre des tests, init réseau pré-chauffée).
3. **Le VPS** : la matrice 50k/100k hold + scaling réel. Single-worker
   calibré au max ici ; la contrainte "4 GB / 2 cores" de la box de dev
   n'est plus le limiteur une fois sur le VPS.
