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

## Tests

```sh
cargo test -p myproxy
```

## Benchmark

```sh
cd bench
./run-bench.sh myproxy on   # ou 3proxy / off
```

Résultats bruts dans `bench/results/<proxy>/auth-<on|off>/<timestamp>/results.txt`.
Comparatif complet (M5→M8, delayed-ACK 3proxy vérifié à la capture) :
`bench/RESULTS.md`.
