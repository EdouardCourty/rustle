# myproxy

Proxy forward HTTP ultra-minimaliste et performant en Rust. Le client configure
`http://IP_DU_VPS:8080` ; les requêtes HTTP/HTTPS passent par le proxy.

- HTTP forward proxy + HTTPS via `CONNECT` (tunnel TCP)
- Auth par API key (`Proxy-Authorization: Basic`)
- V1 : single worker, relay `copy_bidirectional`, limites de connexions, timeouts

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

## Benchmark

```sh
cd bench
./run-bench.sh myproxy on   # ou 3proxy / off
```

Résultats dans `bench/results/<proxy>/auth-<on|off>/summary.txt`.
