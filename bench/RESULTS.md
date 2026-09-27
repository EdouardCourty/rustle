# Résultats M5 — myproxy vs 3proxy (première passe)

Date : 2026-09-27 · Matrice : `{myproxy, 3proxy} × auth=on`, médiane de 3 runs
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

## Tableau comparatif (médiane, auth on)

| Workload | myproxy | 3proxy | Verdict |
|---|---|---|---|
| HTTP 1 KB, keep-alive ×16 | **10 574 req/s** · p50 **1,5 ms** · 96 % CPU | 388 req/s · p50 **41 ms** | **myproxy ×27** (voir caveat 1) |
| HTTP 10 KB, keep-alive ×16 | 8 992 req/s (92 MB/s) · 94 % CPU | **12 499 req/s** (128 MB/s) | 3proxy +39 % |
| HTTP 1 MB, keep-alive ×16 | 604 req/s (634 MB/s) | **2 681 req/s (2 811 MB/s)** | **3proxy ×4,4** |
| HTTP 1 KB, 1 req/conn ×64 | **4 162 req/s** · p50 **7,6 ms** | 3 048 req/s · p50 15,4 ms | **myproxy +37 %**, latence ÷2 |
| CONNECT flood ×8 | 4,86 Gbps · 10,7 s CPU / 10 s | **18,67 Gbps** · 4,2 s CPU / 10 s | **3proxy ×3,8 débit, ×10 CPU-eff.** |
| Hold 10 000 tunnels | **1 703 conn/s** · 0 échec | 1 592 conn/s · 0 échec | myproxy +7 % |
| Hold 25 000 tunnels | **25 000/25 000 ×3** · 3 427 conn/s | 16 903/25 000 · 8 097 échecs | **myproxy** (mur ~17k chez 3proxy, caveat 3) |
| RAM / tunnel idle (hold) | ~18 KB/conn (RSS 182 MB @10k) | **~0 KB/conn** (RSS 3,4 MB plat) | **3proxy** (buffers paresseux) |

## CPU par core (proxy seul, core 0)

- myproxy : ~0,46 Gbps/core en tunnel ; ~1 100 req/s/% CPU en HTTP 1 KB.
- 3proxy : ~4,4 Gbps/core en tunnel ; (HTTP 1 KB non comparable, cf. caveat 1).

## Conclusions de la première passe

1. **Le chemin d'établissement de myproxy est déjà bon** : accept+parse+auth+
   connect-upstream+close. Meilleur que 3proxy en req/s et en latence sur les
   petits objets et les connexions courtes, et seul à tenir 25 000 tunnels
   simultanés sur cette machine.
2. **Le data plane de myproxy est LE bottleneck** (10× moins efficient CPU que
   3proxy en relay tunnel : `copy_bidirectional` + buffers 8 KB → ~2 syscalls
   / 8 KB). C'est la cible M6 nº1 : buffers 64 KB, boucle de relay à buffer
   partagé, `splice()`.
3. **La RAM/tunnel idle** (~18 KB vs ~0) est la cible M6 nº2 : allocation
   paresseuse des buffers de relay au premier octet (ou pool).

## Caveats honnêtes

1. Le 388 req/s de 3proxy sur HTTP 1 KB vient avec p50 = 41 ms ≈ **delayed-ACK
   (40 ms)** : suspicion de Nagle/write-pattern dans 3proxy sur les petites
   réponses. Le ×27 est réel dans ces conditions, mais 3proxy mérite une
   vérification (TCP_NODELAY/writev) avant d'être publié.
2. RAM myproxy : RSS inclut des pages rendues à l'allocateur mais non
   retournées à l'OS (buffers de parse libérés après CONNECT). Un bump à
   mimalloc/jemalloc pourrait changer la donne.
3. 3proxy à 25k : accepte puis ferme sans répondre ("proxy closed before
   CONNECT response") au-delà de ~17k tunnels. Config best-effort (`maxconn
   200000`) — 3proxy a beaucoup de knobs ; à creuser avant de conclure.
4. Le client et le sink partagent le core 2 : les taux d'ouverture hold
   (~1 700-3 400 conn/s) plafonnent sur le harnais, pas sur le proxy seul.
5. Machine 2 cores : la matrice 50k/100k et le scaling 1/2/4/8 cores se font
   sur le VPS cible avec ce même harnais.

## Backlog M6 (piloté par ces chiffres)

1. **Relay tunnel** : buffers 64 KB, réduction des syscalls, essai `splice()`
   → objectif : fermer les ×10 de CPU-eff. (3proxy : 4,4 Gbps/core).
2. **Buffers paresseux** au premier octet → RAM/conn idle ≈ 0.
3. **HTTP bulk (1 MB)** : même chantier que nº1 (634 → ~2 800 MB/s).
4. **Cache DNS** côté proxy (le harnais bypass le DNS ; un vrai workload
   CONNECT hostname en a besoin).
5. Multi-worker (`workers = N`) + `SO_REUSEPORT` + courbe de scaling cores.
6. Vérification du point 3proxy/delayed-ACK pour honnêteté du ×27.
