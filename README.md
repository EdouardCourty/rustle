# myproxy

Proxy forward HTTP ultra-minimaliste et performant en Rust. Le client configure
`http://IP_DU_VPS:8080` ; les requêtes HTTP/HTTPS passent par le proxy.

- HTTP forward proxy + HTTPS via `CONNECT` (tunnel TCP)
- Auth par API key (`Proxy-Authorization: Basic base64(user:key)`)
- Pool upstream keep-alive, relay `splice()` zéro-copie, cache DNS,
  limites de connexions, timeouts

## Layout du projet

```text
myproxy/                 Le serveur (crate binaire)
├── src/
│   ├── main.rs          Boot: tracing, config, runtime tokio -> server::run
│   ├── config/          Types de config + parsing toml
│   ├── http/            Parsing pur (pas de réseau)
│   │   ├── request.rs   Tête de requête + classification (CONNECT/HTTP)
│   │   └── response.rs  Tête de réponse + bornage du corps
│   ├── proxy/           Le métier
│   │   ├── mod.rs       FSM: une tâche par client, keep-alive, routing
│   │   └── pipeline.rs  Chemin HTTP: pool upstream, forward, réponse, pooling
│   ├── relay/           Transports purs (indépendants du métier)
│   │   ├── adaptive.rs  Copy 8→64 KB, pages virtuelles lazy
│   │   ├── splice.rs    Zéro-copie kernel (Linux, splice/pipes)
│   │   └── chunked.rs   Framing chunked relais transparent
│   ├── server/          Accept loop: semaphore, 503, backlog 65535
│   ├── auth.rs          Basic auth temps constant
│   ├── dns.rs           Cache DNS TTL
│   └── upstream.rs      Pool de connexions upstream (LIFO par origin)
└── config.toml.example

bench/                   Le harnais de comparaison
├── generator/           Charge: bench-client
│   └── src/
│       ├── main.rs      CLI + warmup/médiane + ligne RESULT
│       ├── modes/       http | flood | hold | sink
│       ├── metrics.rs   Métriques, percentiles, own_rss
│       └── connect.rs   Dial helpers (CONNECT, résolution unique)
├── dut/                 Device under test: myproxy/, 3proxy/ (Dockerfiles + configs)
├── upstream/            nginx 1kb/10kb/1mb/10mb
├── run-bench.sh         Matrice: pinning CPU auto, CPU/RAM capture, médiane ×3
└── RESULTS.md           Tableaux comparatifs + backlog piloté par les chiffres
```

## Build

```sh
cargo build --release -p myproxy
```

## Run

```sh
./target/release/myproxy myproxy/config.toml.example
```

Test rapide :

```sh
curl -x http://127.0.0.1:8080 -U bench:secret1 http://example.com
```

Proxy HTTPS / HTTP/2 (section `[tls]` renseignée, ex. `listen = "0.0.0.0:8443"`) :

```sh
curl --proxy https://127.0.0.1:8443 --proxy-insecure -U bench:secret1 http://example.com
curl --proxy https://127.0.0.1:8443 --proxy-insecure --proxy-http2 -U bench:secret1 https://example.com
```

SOCKS5 (section `[socks5]`, mot de passe = clé API ; CONNECT + UDP ASSOCIATE pour QUIC/DNS) :

```sh
curl --socks5-hostname u:secret1@127.0.0.1:1080 https://example.com
```

Multi-core : `[server] workers = N` (0 = un shard par CPU, `SO_REUSEPORT`).
Sécurité (proxy personnel) : allowlist d'IP clientes (`[auth] allow_clients`,
ex. la plage de ton VPN), destinations privées refusées par défaut
(`[filter] block_private`), ports autorisés (`allowed_ports`), ban des IP
après N mauvaises clés (`[auth] max_failures`), deadline totale sur le
head (`timeouts.request_head`).

## Tests

```sh
cargo test -p myproxy
```

## Make (résumé des cibles)

```sh
make help              # lister les cibles
make build             # build release (myproxy + bench-client)
make test              # tests unitaires
make images            # images Docker (sérialisé, box 4 GB friendly)
make up / make down    # stack upstream+sink+proxy
make smoke             # test http rapide via la stack
make latest            # afficher la dernière passe de résultats
```

## Benchmark

```sh
make bench                    # myproxy, auth on (défaut)
make bench P=3proxy           # matrice 3proxy
make bench A=off P=3proxy     # sans auth
make bench P=myproxy A=off R=5
make bench-all                # les 4 matrices en séquence (~1 h)
```

Variables : `P` = proxy testé (`myproxy`|`3proxy`|`squid`|`gost`|`tinyproxy`),
`A` = auth (`on`|`off`), `R` = répétitions (défaut 3, médiane calculée).

Transports et multi-core (variables d'environnement de `bench/run-bench.sh`) :

```sh
PROXY_TLS=1 ./bench/run-bench.sh myproxy on    # proxy HTTPS (myproxy, squid, gost)
PROXY_H2=1  ./bench/run-bench.sh gost on       # HTTP/2 (myproxy, gost)
NCPUS="1 2 4 8" make bench-scaling             # courbe multi-core (VPS)
```

Résultats bruts dans `bench/results/<proxy>[-tls|-h2]/auth-<on|off>/<timestamp>/results.txt`.
Comparatif complet (M5→M10, delayed-ACK 3proxy vérifié à la capture) :
`bench/RESULTS.md`.
