# AGENTS.md — rust-proxy

Proxy HTTP/HTTPS (CONNECT) forward en Rust, architecture Tokio.
**Objectif : battre tous les proxies open source en throughput, d'abord sur 1 core, puis multi-core.** Référence actuelle : 3proxy.

Langue : commentaires, commits, README et RESULTS.md en **français** ; identifiants de code en anglais.

## Layout

```
Cargo.toml            workspace (myproxy + bench/generator + e2e), [profile.release] lto=fat, codegen-units=1 ; [profile.e2e] (release sans LTO) pour le harnais e2e
Makefile              help build test e2e e2e-quick fmt check clippy images up down smoke bench bench-all bench-scaling latest clean
myproxy/              le proxy
  config.toml.example
  src/
    main.rs           tracing (RUST_LOG, sur stderr), config = argv[1] (défaut config.toml), server::start puis attente SIGTERM/SIGINT
    config/mod.rs     TOML → structs (humantime pour les durées)
    server/mod.rs     shards (thread + runtime current_thread + listeners SO_REUSEPORT clair + TLS + pool), handshake TLS puis dispatch h2/h1 selon ALPN, compteur global max_connections, admin /health, trim, shutdown gracieux 5 s
    stream.rs         trait ClientStream (TcpStream | TlsStream) : tcp()/into_tcp() = chemins splice/QUICKACK, TLS = copie user-space ; ReadAhead sous rustls (qui lit par 4 KB) : buffer 64 KB adaptatif, seulement pour les flux soutenus
    tls.rs            rustls (ring) : cert/clé PEM, ALPN h2 + http/1.1, AES-128-GCM en tête + ordre serveur, tickets stateless, extraction des secrets si kTLS
    ktls_offload.rs   kTLS : sonde au démarrage (TCP_ULP), vidage rustls (CorkStream), clés → SOL_TLS ; KtlsClient (octets déjà déchiffrés d'abord, EIO = record de contrôle = fin, close_notify par sendmsg) ; tcp()/into_tcp() dès que plus rien n'est en buffer → splice sur le port TLS
    proxy/mod.rs      handle_conn<C: ClientStream> : boucle keep-alive serve() (read_head → auth → classify → CONNECT | HTTP), tunnel monté après
    proxy/socks5.rs   SOCKS5 (RFC 1928/1929) : CONNECT (relais splice) + UDP ASSOCIATE, mot de passe = clé API, BIND refusé
    proxy/socks5_udp.rs UDP ASSOCIATE (QUIC/HTTP3, DNS) : socket côté client + socket dual-stack sortante ; datagrammes acceptés de l'IP du contrôle TCP seulement, réponses seulement des destinations contactées, même policy que TCP ; vit tant que le contrôle TCP
    proxy/h2.rs       HTTP/2 (ALPN h2) : 1 tâche/stream ; CONNECT = tunnel h2 ↔ TCP (flow control) ; autres méthodes → HTTP/1.1 via le pool, chunked décodé
    proxy/pipeline.rs http() : forward_request, retry, 1xx, framing de body, pooling amont
    http/request.rs   RequestHead = Spans dans le buffer (parse zéro-alloc), classify, parse_authority
    http/response.rs  parse head réponse, keep_alive, framing
    relay/mod.rs      bidirectional (splice sur Linux), IdleTimer (timeout d'inactivité)
    relay/adaptive.rs copy adaptatif 8→64 KB, puis bascule splice pour le reste du body ; splice immédiat si le corps restant déclaré ≥ 128 KB
    relay/splice.rs   pipe2 + splice via TcpStream::try_io, PipePair 1 MB (pool par shard, 16 max, retour si vide ; pipes < 64 KB refusés → degraded_pipes), tunnel() paresseux (copie adaptative puis pipe si soutenu), dir() (fill-then-drain)
    relay/chunked.rs  relais chunked transparent (framing verbatim), dans les deux sens : réponses upstream→client et uploads client→upstream
    auth.rs           Basic auth, mot de passe ∈ api_keys (ct_eq sur toutes les clés, sans court-circuit), base64 sur pile
    accesslog.rs      log d'accès : niveaux off/errors/all, lignes clé=valeur horodatées, canal borné try_send (jamais bloquant, drops comptés), thread writer (flush 200 ms, refus à l'accept agrégés 1/s, réouverture SIGHUP)
    policy.rs         DestPolicy (ports autorisés avant DNS, plages non publiques + deny CIDR sur l'adresse résolue) ; ClientPolicy (allowlist d'IP clientes, à l'accept) ; AuthLimiter (bans par IP après N mauvaises clés, sans verrou tant que personne n'échoue)
    dns.rs            cache positif, TTL (hors verrou), Mutex<HashMap>
    upstream.rs       pool keep-alive par origine (LIFO)
e2e/                  tests fonctionnels boîte noire (voir e2e/README.md) : le binaire release lancé par scénario (config générée, ports lus sur la ligne `myproxy listening`), origines/cibles locales programmables, clients HTTP/1.1, TLS, h2, SOCKS5, curl
  src/harness/        proxy.rs (Config, Proxy : spawn, /proc, signaux, finish = fuite CLOSE_WAIT + SIGTERM propre), origin.rs, targets.rs, payload.rs, certs.rs
  src/clients/        http1.rs, h2.rs, socks5.rs, curl.rs
  src/suites/         un fichier par feature : matrix http connect h2 socks5 uploads limits security robustness process logs interop_curl
bench/
  run-bench.sh <proxy> <on|off>            matrice (proxy = myproxy|3proxy|squid|gost|tinyproxy)
  run-campaign.sh                          campagne complète (5 proxies × clair/TLS/h2) → results/campaign-<ts>.txt ; tables : tools/campaign-table.py
  run-scaling.sh                           courbe multi-core (PROXIES, NCPUS, AUTH, PROXY_TLS, PROXY_H2) — VPS
  env.sh, docker-compose.yml               cpuset : proxy sur PROXY_CPU, le reste sur BENCH_CPU
  generator/                               crate bench-client (http | flood | hold | sink)
  dut/<proxy>/                             Dockerfiles + configs on/off (user bench / clé secret1, port 8080) ; ligne `# bench-ports: tls=<p> h2=<p>` = modes supportés (cert ECDSA P-256 auto-signé généré au build) ; WORKERS lu à l'entrypoint (myproxy, squid) ; myproxy/Dockerfile.debug = strace
  upstream/                                nginx (objets 1kb/10kb/1mb/10mb)
  tools/local-upstream-http11.py           upstream HTTP/1.1 keep-alive local (up11.py)
  docker-compose.seccomp.yml               overlay `SECCOMP=unconfined` (io_uring ; l'appliquer à tous les DUT comparés)
  RESULTS.md                               synthèse publiée, historique des milestones
  results/                                 sorties brutes (gitignored)
```

## Architecture (ce qu'il faut savoir)

- **Threading** : shared-nothing. `workers` = N shards (0 = un par CPU) : chacun un thread OS, un runtime `current_thread`, un listener `SO_REUSEPORT` sur le même port (le noyau répartit), son pool amont. Une connexion reste sur son shard. Partagé : `Config`, `Authenticator`, `DnsCache` (Mutex, froid), compteurs atomiques (`active` = max_connections global, `peak`, `accepted` par shard loggé au shutdown). `pin_cores` optionnel (un shard par CPU → steering `SO_ATTACH_REUSEPORT_CBPF` : connexion vers le shard du CPU qui a reçu le SYN, sinon hash). Shard 0 : admin + `malloc_trim` après rafales. NODELAY + `TCP_DEFER_ACCEPT` + buffers (`socket_buffer_bytes`) posés sur le listener (hérités). `accepted` par shard sur sa propre ligne de cache ; `ConnSlot` tient l'`Arc<Services>` du shard (pas de refcount partagé). Le main attend le signal puis `shutdown_and_join`.
- **Transports client** : port clair (HTTP/1.1, splice) et port TLS optionnel (`[tls]`) : HTTP/1.1 via le même pipeline générique (`ClientStream`, monomorphisé : le chemin clair est compilé à l'identique) ou HTTP/2 si l'ALPN négocie `h2`. En TLS : pas de splice (le socket porte des records), `flush()` à chaque fin d'écriture (rustls peut garder du chiffré en buffer), `close_notify` en fin de connexion. Côté upstream : toujours TCP clair HTTP/1.1.
- **Policy** : `connect_upstream` refuse (403 / SOCKS REP 0x02) un port hors allowlist puis toute adresse résolue non publique ou dans `deny` (protège du DNS rebinding) ; appliqué à CONNECT, HTTP, h2 et SOCKS5. Une IP hors `allow_clients` ou bannie (mauvaises clés) est fermée à l'accept, avant toute lecture. Une connexion = un `Arc<Services>` (config, auth, DNS, policy, limiter, pool du shard).
- **CONNECT** : clé `host:port` → DNS → connect → `200` → early bytes → `relay::bidirectional` (splice::tunnel : par sens, copie 8→64 KB puis PipePair du pool si soutenu ; half-close via `shutdown(SHUT_WR)`). L'état keep-alive de `serve` est boxé : la future d'un tunnel tenu ne garde que l'état du relais (test `connection_future_stays_small`).
- **HTTP** : clé d'origine → `pool.take` ou connect → `forward_request` (réécriture du head dans `rewrite_buf`, `write_all` 1 syscall si sans body, `write_vectored` head+prefix sinon) → `read_response_head` (httparse, 64 headers) → body selon framing (CL borné / chunked / EOF) → remise au pool si complet + keep-alive.
- **Retry** sur socket poolée périmée : GET/HEAD avec body entièrement bufferisé uniquement.
- **Uploads** : `Transfer-Encoding: chunked` accepté (seul, sans Content-Length, pas en HTTP/1.0 → sinon 400) et relayé verbatim (staging = `rewrite_buf`), budget appliqué pendant le relais, octets après le chunk final → close après la réponse ; non rejouable. En h2, un body sans Content-Length part en chunked vers l'upstream.
- **Limites** : `max_request_body_bytes` → 413, `max_response_body_bytes` → 502 (CL déclaré) ou coupure (inconnu).
- **Syscalls** mesurés : ~2 recv + 2 send / requête, epoll_ctl ≈ 0. NODELAY 1×/socket, QUICKACK 1×/connexion, SO_SNDBUF/RCVBUF 1 MB, pipes 1 MB.

## Config (TOML, seul input ; pas de flags CLI, pas de reload)

| Clé | Défaut |
|---|---|
| `server.listen` | `0.0.0.0:8080` (SocketAddr littéral) |
| `server.workers` / `pin_cores` | 1 (0 = un shard par CPU) / false |
| pool amont | limites **par shard** |
| `server.admin_listen` | `127.0.0.1:8181` (`""` = off, `GET /health` seulement) |
| `server.socket_buffer_bytes` | 1048576 (0 = autotuning noyau ; taille fixe = autotuning off, plafonnée par `rmem_max`/`wmem_max`, warn au démarrage ; M21.9 : 1 MB gagne sur le bench LAN, 0 conseillé pour des clients lointains) |
| `auth.api_keys` | `[]` → **auth off = open relay** |
| `limits.max_connections` / `max_header_bytes` | 10000 / 65536 |
| `timeouts.connect` / `idle` | 5s / 60s |
| `dns.ttl` / `max_entries` | 60s (0 = off) / 4096 |
| `http.upstream_keepalive` / `max_idle_per_origin` | true / 256 (cap global 1024 par shard en dur) |
| `http.max_request_body_bytes` / `max_response_body_bytes` | 0 = illimité |
| `tls.listen` / `cert` / `key` | `""` (off) / PEM |
| `tls.handshake_timeout` / `http2` / `ktls` | 10s / true (ALPN h2) / **false** (opt-in : +13 % tunnels, −30 % RAM, mais −37 % bulk HTTP ; M17) |
| `auth.allow_clients` | [] (vide = tous ; CIDR, fermé à l'accept sur tous les listeners) |
| `auth.max_failures` / `failure_window` / `ban_duration` | 5 / 1m / 15m (0 = off ; sans credentials ne compte pas) |
| `timeouts.request_head` | 10s (total head / handshake SOCKS5, anti-slowloris) |
| `filter.block_private` / `deny` / `allowed_ports` | **true** / [] / [] (vide = tous) — les configs de bench mettent false (réseau Docker privé) |
| `socks5.listen` | `""` (off) |
| `log.access` / `output` / `file` / `target` | errors / file (ou stdout) / `""` (off) / host (ou url) — bench : off, `ACCESS_LOG=all` pour mesurer |

`deny_unknown_fields` sur toutes les sections : une clé inconnue ou mal placée fait échouer le chargement.

## Commandes

```bash
cargo build --release -p myproxy         # pas de target-cpu=native en local (seulement dans le Dockerfile bench)
cargo test -p myproxy                    # tests inline (#[cfg(test)]) dont bout en bout dans server/mod.rs
make check                               # fmt + clippy -D warnings + tests + e2e --quick (~20 s) — avant chaque commit
make e2e [F="filtre"]                    # suite e2e complète (~2 min, ~530 scénarios) — avant chaque milestone
cargo run --profile e2e -p e2e -- --list # scénarios ; --keep garde les répertoires (config, logs)
make bench P=myproxy A=off R=3           # matrice complète (voir ci-dessous)
BENCH_ONLY=<test> BENCH_SEED=2004113323 bench/run-bench.sh myproxy off
```

Features optionnelles : `jemalloc`, `mimalloc` (exclusives ; défaut = malloc glibc).

## Benchmarks — règles

- **Protocole M21.5** : warmup dans un run séparé (`--warmup-only`) avant l'échantillonnage → chiffres ~6–8 % plus bas qu'avant, ne pas comparer aux campagnes antérieures. Compteurs TCP du netns par run : `segs_out_per_run`, `active_opens_per_run` (churn du pool), `retrans_per_run`, `delayed_acks_per_run`, `listen_overflows`, `listen_drops`. `flood-128` hors matrice (`BENCH_ONLY`) : au-delà du budget noyau de pipes 1 MB.
- A/B : alterner base/variante (2–3 passes), jamais une variante seule ; ne rien modifier dans le repo pendant un A/B (l'image est reconstruite à chaque run).
- Seed de référence **`2004113323`**. Ordre des tests mélangé (LCG), proxy redémarré avant chaque test. Bruit ±3–5 % : en dessous, pas de conclusion.
- Tests : http-1kb (64 conns), http-10kb (16), http-1mb (16), http-10mb (8), http-1kb-short (sans keep-alive), flood-8, hold 1k/5k/10k/25k.
- Sortie : lignes `RESULT median ...` (rps, mbps, p50, errors, `err_kinds`, `reconnects`, CPU proxy/generator/upstream, `rss_mb` = résiduel après le test, `peak_rss_mb` = VmHWM = coût réel). CPU proxy = cgroup du conteneur, RSS = somme des processus (DUT multi-process comme Squid SMP inclus).
- Transports : `PROXY_TLS=1` (HTTPS proxy) / `PROXY_H2=1` (HTTP/2 via TLS) → résultats sous `results/<proxy>-tls|-h2/`. TLS : myproxy, Squid, gost. h2 : myproxy, gost (3proxy/Tinyproxy : clair seulement). Générateur : `--proxy-tls`, `--proxy-h2` (1 connexion h2 par conn, requêtes séquentielles ; CONNECT = 1 stream sur sa propre connexion), certificat non vérifié mais signatures du handshake vérifiées.
- `KTLS=true|false` (myproxy, port TLS), `SECCOMP=unconfined` (io_uring).
- Multi-core : `PROXY_NCPU=<n>` (n premiers CPU au proxy, le reste au harnais) ou `PROXY_CPU`/`BENCH_CPU` explicites ; `WORKERS=<n>` ; `GEN_THREADS` = threads du générateur (à dimensionner sur BENCH_CPU). En-tête de results.txt = paramètres du run.
- Générateur : honore `Connection: close`/HTTP/1.0 (reconnexion) et rejoue une fois un GET sur connexion réutilisée fermée avant tout octet (RFC 9112 §9.3.1) → `reconnects` (Tinyproxy : 1 par requête, il ferme sans l'annoncer).
- Métrique à suivre : **CPU proxy par requête** (`cpu_s_per_run / (rps × durée)`), le débit étant plafonné par le harnais. Bruit entre deux matrices identiques : jusqu'à ±7 % CPU, ±15 % sur flood.
- Répartition mesurée (1kb) : user ≈ 9 µs/req, sys ≈ 35 µs/req. Syscalls au plancher. Profil : image `dut/myproxy/Dockerfile.debug` (strace). Pas de perf sur la box (paranoid=4).
- `ALLOCATOR="--features mimalloc" ./run-bench.sh …` pour un A/B allocateur (retesté : neutre). **`err_kinds` doit être vide.**
- Ne pas builder les images en parallèle (OOM sur 4 GB). Le harnais partage 2 cores : plafond ~23 µs/req côté generator+nginx.
- Le ×9.5 vs 3proxy sur 1 KB vient du delayed-ACK 40 ms de 3proxy : **ne jamais le publier sans ce caveat**.
- 1 KB est limité par la latence du harnais (~15k rps) ; le proxy seul est estimé à 21–24k rps (~42–47 µs/req).
- Ne pas comparer des chiffres entre changements de protocole de bench.
- Multi-core : non mesuré (harnais = 2 cores). Nécessite une machine ≥ 2 × N cores.
- Ne jamais compiler ni modifier `run-bench.sh` pendant un bench (CPU partagé ; bash relit le script en cours d'exécution).

## Conventions

- Commits : `M<major>.<minor>[-<n>]: <résumé> — <détails chiffrés>`, en français, gated par une matrice au seed fixe. Attribution Claude en trailer.
- Tests : `make check` (e2e rapide inclus) avant chaque commit, `make e2e` complet avant chaque milestone et après tout changement du proxy (relais, pipeline, policy, TLS). Nouvelle feature = nouveau scénario dans `e2e/src/suites/<feature>.rs` (et dans la matrice si elle dépend d'une config).
- Tags baseline : `m10.3-tokio`, `m11.1-fsm`. Labels de review dans le code/commits : B1–B8, R3, O1, O2.
- Pas de tags de milestone (`M10.x`) dans les commentaires du code : ils vont dans les commits et RESULTS.md.
- `unsafe` cantonné à libc/fd (commentaire `SAFETY` attendu sur tout nouveau bloc) : `relay/splice.rs` (pipes, splice, dup), `server/mod.rs` (setsockopt, affinité CPU, malloc_trim), `ktls_offload.rs` (TCP_ULP, SOL_TLS, sendmsg close_notify), `stream.rs` (ReadAhead : set_len après lecture), `http/request.rs::span_of` (`offset_from`).
- Erreurs : réponses statiques (`&[u8]` 400/407/413/431/502/504), I/O errors largement ignorées (`let _ =`), `anyhow` uniquement au démarrage.

## Invariants — ne pas casser

1. **Hot path zéro-alloc** : réutiliser `buf`, `resp_buf`, `rewrite_buf`, `origin_key` (locaux à la connexion), pipes via le pool du shard. Pas de `setsockopt` par requête (régression M10.3 : `set_nodelay` par requête).
2. **`RequestHead` = Spans dans `buf`** : `buf.clear()`, `take`, realloc entre parse et usage les invalident. `span_of` suppose que httparse renvoie des sous-slices du même buffer.
3. **splice `dir()`** : toujours drainer le pipe s'il contient des données avant de parquer sur `src.readable()` (sinon deadlock). Sur le chemin HTTP, fds `dup`és partagent la file description : EOF ne doit pas half-close le client.
4. **Retry** : idempotent + body rejouable uniquement (B7). Ne pas élargir.
5. Pool : n'accepter qu'une réponse complète, keep-alive, framée, sans sur-lecture.
6. `m11.1-fsm` = FSM epoll abandonné (CPU/req identique à tokio, rps pire). Ne pas merger, ne pas y revenir sans donnée nouvelle.
7. Gains < 5 % = bruit. Toute affirmation perf passe par la matrice au seed fixe.
8. io_uring (M18, prototype monoio, supprimé du repo — récupérable au commit `a2b11e3`) : ±8 % selon la taille, pas de gain net sur 1 core ; kTLS (M17) : opt-in, gagnant tunnels/RAM, perdant bulk HTTP. Ne pas changer ces décisions sans nouvelle mesure (multi-core).

## État connu (2026-10-03, branche `m10.4-syscalls`)

- `make check` vert : fmt, clippy `-D warnings` (workspace, e2e compris), `cargo test -p myproxy` 63/63 (unitaires + bout en bout dans le process), e2e `--quick` 62 scénarios. `make e2e` : 524 OK + 3 SKIP (curl 7.88 sans `--proxy-http2`) sur 527, ~2 min, 3 runs d'affilée sans flake. Pas de CI.
- Fait depuis M10.4 : correctifs chunked, current_thread + shards SO_REUSEPORT (M12), proxy HTTPS + HTTP/2 (M13), policy/allowlist/bans/slowloris/SOCKS5 (M14), uploads chunked + SOCKS5 UDP (M15), log d'accès (M16), kTLS opt-in (M17), prototype io_uring sans gain (M18), fenêtres h2 (M20), passe d'optimisations M21 (splice/pipes, TLS AES-128, h2 sans copie, pool 256, DEFER_ACCEPT, correctifs multi-core, buffers configurables, protocole de bench M21.5).
- Campagne de référence : 2026-10-03 (`results/campaign-20261003-015025.txt`), protocole M21.5.
- Le bench (nginx statique, Content-Length) n'exerce ni le chunked, ni SOCKS5, ni les uploads : couverts par les tests et la suite e2e.
- Suite e2e (M22) : un comportement attendu = un scénario. Un bug trouvé → scénario qui le reproduit d'abord (il doit échouer), puis correctif. Mutations vérifiées : half-close du splice, suppression de `Proxy-Authorization`, `allowed_ports` → détectées.
- Campagne de référence : `bench/run-campaign.sh` (5 proxies × clair/TLS/h2), tableaux par `bench/tools/campaign-table.py`, synthèse dans `bench/RESULTS.md` et le README.

## Backlog / points d'amélioration

**Perf**
- **Multi-core : mesurer** (`make bench-scaling` sur une machine ≥ 2 × N cores) — rien n'est mesuré au-delà d'un core. Retester io_uring et kTLS dans ce cadre.
- Chemin h2 : plus lent que TLS+HTTP/1.1 sur le bulk (copie `Bytes` par chunk, framing). Flood h2 devant gost depuis M20 (fenêtres 4/16 MB), borné par le core du harnais.
- kTLS : comprendre la perte sur le bulk HTTP (−37 % à 1 MB) — `perf` indisponible sur la box actuelle.
- Pool : pas de reaper d'inactifs, cap 1024/shard en dur. DNS : vidage complet à saturation, pas de cache négatif ni de coalescing.
- PGO/BOLT, `panic=abort` (écarté : une panique tuerait tout le shard), `target-cpu=native` hors Docker.

**Fonctionnel (non supporté)**
- Requêtes pipelinées côté client (fermeture après la réponse), `Upgrade` (WebSocket sur `http://` en clair), `Expect: 100-continue` complet, HTTP/3 vers le proxy (MASQUE).
- Rechargement à chaud (config, certificat) : redémarrage nécessaire.
- Côté réponse amont : `Content-Length` dupliqués non vérifiés (origine supposée de confiance).

**Qualité**
- CI (`make check`). `Mutex::lock().unwrap()` → poison = panic. Pas de `setrlimit` (prévoir `LimitNOFILE` en déploiement : ~6 fds par tunnel splice).
