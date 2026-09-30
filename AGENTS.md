# AGENTS.md — rust-proxy

Proxy HTTP/HTTPS (CONNECT) forward en Rust, architecture Tokio.
**Objectif : battre tous les proxies open source en throughput, d'abord sur 1 core, puis multi-core.** Référence actuelle : 3proxy.

Langue : commentaires, commits, README et RESULTS.md en **français** ; identifiants de code en anglais.

## Layout

```
Cargo.toml            workspace (myproxy + bench/generator), [profile.release] lto=fat, codegen-units=1 (partagé par les 2 binaires)
Makefile              build test fmt clippy images up down smoke bench bench-off bench-3proxy bench-all latest clean
myproxy/              le proxy (~2850 lignes)
  config.toml.example
  src/
    main.rs           runtime tokio multi_thread(workers), tracing (RUST_LOG), config = argv[1] (défaut config.toml)
    config/mod.rs     TOML → structs (humantime pour les durées)
    server/mod.rs     bind (socket2, listen 65535), accept loop, Semaphore(max_connections), admin /health, shutdown gracieux 5 s
    proxy/mod.rs      handle_conn : FSM par connexion (read_head → auth → classify → CONNECT | HTTP)
    proxy/pipeline.rs http() : forward_request, retry, 1xx, framing de body, pooling amont
    http/request.rs   RequestHead = Spans dans le buffer (parse zéro-alloc), classify, parse_authority
    http/response.rs  parse head réponse, keep_alive, framing
    relay/mod.rs      bidirectional (splice sur Linux), IdleTimer (timeout d'inactivité)
    relay/adaptive.rs copy adaptatif 8→64 KB, puis bascule splice pour le reste du body
    relay/splice.rs   pipe2 + splice, PipePair 1 MB, AsyncFd, dir() (fill-then-drain)
    relay/chunked.rs  relais chunked transparent (framing verbatim)
    auth.rs           Basic auth, mot de passe ∈ api_keys (ct_eq), base64 sur pile
    dns.rs            cache positif, TTL, Mutex<HashMap>
    upstream.rs       pool keep-alive par origine (LIFO)
bench/
  run-bench.sh <myproxy|3proxy> <on|off>   harnais complet
  env.sh, docker-compose.yml               cpuset : proxy sur PROXY_CPU, le reste sur BENCH_CPU
  generator/                               crate bench-client (http | flood | hold | sink)
  dut/{myproxy,3proxy}/                    Dockerfiles + configs on/off (+ Dockerfile.debug = strace)
  upstream/                                nginx (objets 1kb/10kb/1mb/10mb)
  tools/local-upstream-http11.py           upstream HTTP/1.1 keep-alive local (up11.py)
  RESULTS.md                               synthèse publiée, historique des milestones
  results/                                 sorties brutes (gitignored)
```

## Architecture (ce qu'il faut savoir)

- **Threading** : `workers=1` (défaut) → runtime `current_thread` (acceptor + connexions sur un thread) ; `workers>1` → `multi_thread`. 1 accept loop, 1 `tokio::spawn` par connexion. NODELAY + buffers 1 MB posés sur le listener (hérités par les sockets acceptées). `malloc_trim` après les rafales (glibc). Pas de SO_REUSEPORT, pas de runtime par core. Pool amont et cache DNS = `Arc<Mutex<..>>` globaux.
- **CONNECT** : clé `host:port` → DNS → connect → `200` → early bytes → `relay::bidirectional` (2 PipePair splice, half-close via `shutdown(SHUT_WR)`).
- **HTTP** : clé d'origine → `pool.take` ou connect → `forward_request` (réécriture du head dans `rewrite_buf`, `write_all` 1 syscall si sans body, `write_vectored` head+prefix sinon) → `read_response_head` (httparse, 64 headers) → body selon framing (CL borné / chunked / EOF) → remise au pool si complet + keep-alive.
- **Retry** sur socket poolée périmée : GET/HEAD avec body entièrement bufferisé uniquement.
- **Limites** : `max_request_body_bytes` → 413, `max_response_body_bytes` → 502 (CL déclaré) ou coupure (inconnu).
- **Syscalls** mesurés : ~2 recv + 2 send / requête, epoll_ctl ≈ 0. NODELAY 1×/socket, QUICKACK 1×/connexion, SO_SNDBUF/RCVBUF 1 MB, pipes 1 MB.

## Config (TOML, seul input ; pas de flags CLI, pas de reload)

| Clé | Défaut |
|---|---|
| `server.listen` | `0.0.0.0:8080` (SocketAddr littéral) |
| `server.workers` | 1 |
| `server.admin_listen` | `127.0.0.1:8181` (`""` = off, `GET /health` seulement) |
| `auth.api_keys` | `[]` → **auth off = open relay** |
| `limits.max_connections` / `max_header_bytes` | 10000 / 65536 |
| `timeouts.connect` / `idle` | 5s / 60s |
| `dns.ttl` / `max_entries` | 60s (0 = off) / 4096 |
| `http.upstream_keepalive` / `max_idle_per_origin` | true / 32 (cap global 1024 en dur) |
| `http.max_request_body_bytes` / `max_response_body_bytes` | 0 = illimité |

Pas de `deny_unknown_fields` : une faute de frappe est ignorée silencieusement.

## Commandes

```bash
cargo build --release -p myproxy         # pas de target-cpu=native en local (seulement dans le Dockerfile bench)
cargo test -p myproxy                    # tests unitaires inline (#[cfg(test)]), pas de tests d'intégration
cargo clippy -p myproxy                  # doit rester à 0 warning
make bench P=myproxy A=off R=3           # matrice complète (voir ci-dessous)
BENCH_ONLY=<test> BENCH_SEED=2004113323 bench/run-bench.sh myproxy off
```

Features optionnelles : `jemalloc`, `mimalloc` (exclusives ; défaut = malloc glibc).

## Benchmarks — règles

- Seed de référence **`2004113323`**. Ordre des tests mélangé (LCG), proxy redémarré avant chaque test. Bruit ±3–5 % : en dessous, pas de conclusion.
- Tests : http-1kb (64 conns), http-10kb (16), http-1mb (16), http-10mb (8), http-1kb-short (sans keep-alive), flood-8, hold 1k/5k/10k/25k.
- Sortie : lignes `RESULT median ...` (rps, mbps, p50, errors, `err_kinds`, CPU proxy/generator/upstream, `rss_mb` = résiduel après le test, `peak_rss_mb` = VmHWM = coût réel).
- Métrique à suivre : **CPU proxy par requête** (`cpu_s_per_run / (rps × durée)`), le débit étant plafonné par le harnais. Bruit entre deux matrices identiques : jusqu'à ±7 % CPU, ±15 % sur flood.
- Répartition mesurée (1kb) : user ≈ 9 µs/req, sys ≈ 35 µs/req. Syscalls au plancher. Profil : image `dut/myproxy/Dockerfile.debug` (strace). Pas de perf sur la box (paranoid=4).
- `ALLOCATOR="--features mimalloc" ./run-bench.sh …` pour un A/B allocateur (retesté : neutre). **`err_kinds` doit être vide.**
- Ne pas builder les images en parallèle (OOM sur 4 GB). Le harnais partage 2 cores : plafond ~23 µs/req côté generator+nginx.
- Le ×9.5 vs 3proxy sur 1 KB vient du delayed-ACK 40 ms de 3proxy : **ne jamais le publier sans ce caveat**.
- 1 KB est limité par la latence du harnais (~15k rps) ; le proxy seul est estimé à 21–24k rps (~42–47 µs/req).
- Ne pas comparer des chiffres entre changements de protocole de bench.
- Multi-core : non mesuré (harnais = 2 cores). Nécessite le VPS.

## Conventions

- Commits : `M<major>.<minor>[-<n>]: <résumé> — <détails chiffrés>`, en français, gated par une matrice au seed fixe. Attribution Claude en trailer.
- Tags baseline : `m10.3-tokio`, `m11.1-fsm`. Labels de review dans le code/commits : B1–B8, R3, O1, O2.
- Les tags de milestone `M10.x` dans les commentaires sont en cours de retrait (diff non commité) : ne pas en ajouter.
- `unsafe` cantonné à libc/fd : `splice.rs`, `server/mod.rs` (setsockopt), `http/request.rs::span_of` (`offset_from`).
- Erreurs : réponses statiques (`&[u8]` 400/407/413/431/502/504), I/O errors largement ignorées (`let _ =`), `anyhow` uniquement au démarrage.

## Invariants — ne pas casser

1. **Hot path zéro-alloc** : réutiliser `buf`, `resp_buf`, `rewrite_buf`, `origin_key`, `resp_pipes` (locaux à la connexion). Pas de `setsockopt` par requête (régression M10.3 : `set_nodelay` par requête).
2. **`RequestHead` = Spans dans `buf`** : `buf.clear()`, `take`, realloc entre parse et usage les invalident. `span_of` suppose que httparse renvoie des sous-slices du même buffer.
3. **splice `dir()`** : toujours drainer le pipe s'il contient des données avant de parquer sur `src.readable()` (sinon deadlock). Sur le chemin HTTP, fds `dup`és partagent la file description : EOF ne doit pas half-close le client.
4. **Retry** : idempotent + body rejouable uniquement (B7). Ne pas élargir.
5. Pool : n'accepter qu'une réponse complète, keep-alive, framée, sans sur-lecture.
6. `m11.1-fsm` = FSM epoll abandonné (CPU/req identique à tokio, rps pire). Ne pas merger, ne pas y revenir sans donnée nouvelle.
7. Gains < 5 % = bruit. Toute affirmation perf passe par la matrice au seed fixe.

## État connu (2026-09-29, branche `m10.4-syscalls`)

- `cargo test -p myproxy` : 29/29 ; clippy `--all-targets` : 0 warning. Pas de CI.
- `relay/chunked.rs` : relais sur staging = `resp_buf` (zéro alloc), 1 read + 1 write par refill ; reprend les octets lus avec le head de réponse (avant : forwardés bruts puis parser désynchronisé → connexion client bloquée jusqu'à l'idle timeout). Budget `max_response_body_bytes` appliqué pendant le relais, trailers bornés (64 lignes × 1024 B), sur-lecture détectée → pas de pool. Tests : `relay::chunked::tests`.
- Requêtes : `Transfer-Encoding` client, `Content-Length` invalide ou dupliqué contradictoire → 400 (anti-smuggling ; les uploads chunked ne sont pas supportés).
- `config::tests::shipped_configs_parse` valide `config.toml.example` et les configs bench.
- Le bench (nginx statique, Content-Length) n'exerce pas le chemin chunked.

## Backlog / points d'amélioration (par priorité)

**Sécurité / correction (avant toute mise en prod)**
- Côté réponse amont : CL dupliqués non vérifiés (upstream supposé de confiance).
- Pas de filtrage SSRF (loopback, RFC1918, 169.254.169.254, port admin) ni allowlist de ports CONNECT. Auth off par défaut = open relay.
- Slowloris : timeout de head par `read`, pas total ; pas de durée de vie max de connexion.
- fds : pas de `setrlimit`, ~6 fds/tunnel splice vs défaut 1024.
- Auth : `.any()` court-circuite, `ct_eq` fuit la longueur ; pas de rate limit sur échecs.
- Non supportés : requêtes pipelinées client, `Upgrade`, `Expect: 100-continue` complet.

**Perf**
- **Multi-core = le plus gros écart avec l'objectif.** Piste : N runtimes `current_thread` (un par core) + listener `SO_REUSEPORT` chacun + pool/DNS thread-local (supprime Mutex, Arc, Send). Puis mesurer sur VPS multi-core avec pinning.
- Pool : pas de reaper d'inactifs ; cap global 1024 non configurable. DNS : wipe-all à saturation, pas de cache négatif, pas de coalescing, double lock par `get()`.
- PGO/BOLT, `panic=abort`, `.cargo/config.toml` avec `target-cpu=native` pour le VPS.
- io_uring : seulement si < ~4 syscalls/req atteignables ; gain = batching/multishot, pas moins de travail/req ; incompatible avec splice + buffer rings.
- RAM : ~5 KB par tunnel vivant (peak_rss_mb) ; l'ancien « 1,3 KB/tunnel » était du résiduel après fermeture. À recomparer avec 3proxy sur peak_rss_mb.
- Single-core : ~80 % du CPU est noyau ; gains user-space restants (PGO, timers, hash) < bruit du harnais.

**Qualité**
- Tests d'intégration (forward HTTP, chunked, retry du pool, budgets, splice HTTP) — aujourd'hui couverts uniquement par le bench.
- CI (build + test + clippy + fmt). `deny_unknown_fields`. Shims morts : `_unused` dans `pipeline.rs`. `Mutex::lock().unwrap()` → poison = panic.
