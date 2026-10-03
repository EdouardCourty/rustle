# myproxy

Proxy forward haute performance en Rust (Tokio), pensé pour un usage personnel :
HTTP/1.1, HTTPS (CONNECT), proxy HTTPS avec HTTP/2, SOCKS5 (TCP + UDP), multi-core
shared-nothing. Objectif de départ : battre les proxies open source en débit, d'abord
sur un core puis en multi-core. Les chiffres sont dans [Benchmarks](#benchmarks) et
l'historique complet dans [`bench/RESULTS.md`](bench/RESULTS.md).

## Fonctionnalités

**Protocoles**

| Côté client → proxy | |
|---|---|
| HTTP/1.1 (et 1.0) en clair | requêtes `http://` + tunnels `CONNECT` |
| HTTP/1.1 sur TLS (« proxy HTTPS ») | identifiants et requêtes chiffrés jusqu'au proxy |
| HTTP/2 sur TLS (ALPN `h2`) | requêtes et tunnels `CONNECT` multiplexés |
| SOCKS5 | `CONNECT` + `UDP ASSOCIATE` (QUIC/HTTP3, DNS) |

À travers le proxy : tout ce qui passe en TCP via `CONNECT` (HTTPS en HTTP/1.1 ou
HTTP/2, SSH…), HTTP/3/QUIC via SOCKS5 UDP. Vers les origines `http://`, le proxy parle
HTTP/1.1 (pool keep-alive).

**Performance** — un shard par core (`SO_REUSEPORT`, runtime `current_thread`, pool
amont par shard), chemin chaud sans allocation, relais zéro-copie `splice()` pour les
tunnels et les gros corps de réponse, ~2 recv + 2 send par requête.

**Sécurité (proxy personnel)**
- clé API (`Proxy-Authorization: Basic`, ou mot de passe SOCKS5), comparée en temps constant ;
- allowlist d'IP clientes (ex. la plage de ton VPN) : les autres sont fermées dès l'accept ;
- ban temporaire des IP après N mauvaises clés ;
- destinations privées refusées (loopback, RFC 1918, metadata cloud `169.254.169.254`…),
  vérifiées sur l'adresse résolue (DNS rebinding couvert), liste `deny` et ports autorisés ;
- anti-slowloris (délai total pour un head de requête), anti request-smuggling
  (`Transfer-Encoding` / `Content-Length` ambigus → 400).

**Exploitation** — log d'accès (fichier ou stdout, jamais bloquant), `GET /health`
sur un port admin, arrêt gracieux (SIGTERM), réouverture du log sur SIGHUP.

## Démarrage rapide

```sh
cargo build --release -p myproxy
cp myproxy/config.toml.example config.toml     # puis éditer : clé API, listen…
./target/release/myproxy config.toml
```

```sh
# HTTP / HTTPS via le port en clair
curl -x http://127.0.0.1:8080 -U moi:MA_CLE https://example.com

# Proxy HTTPS (section [tls]) — HTTP/1.1 puis HTTP/2
curl --proxy https://proxy.example.org:8443 -U moi:MA_CLE https://example.com
curl --proxy https://proxy.example.org:8443 --proxy-http2 -U moi:MA_CLE https://example.com

# SOCKS5 (section [socks5])
curl --socks5-hostname moi:MA_CLE@127.0.0.1:1080 https://example.com
```

Clé API : une chaîne aléatoire longue (`openssl rand -base64 32`). Le nom
d'utilisateur est ignoré. Sur le port en clair et en SOCKS5, la clé circule **en clair** :
n'exposer ces ports que sur une interface de confiance (loopback, VPN) ; sur Internet,
utiliser le port HTTPS.

### Certificat pour le port HTTPS

`[tls]` attend une chaîne de certificats et une clé privée PEM. Production : Let's
Encrypt sur le nom du proxy (`cert = fullchain.pem`, `key = privkey.pem`). Usage
interne : certificat auto-signé ou CA interne installée sur les postes — les
navigateurs refusent un proxy HTTPS non reconnu. Préférer une clé ECDSA P-256 (handshake
nettement moins coûteux que RSA) :

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -days 365 -subj /CN=proxy.example.org -keyout key.pem -out cert.pem
```

Le certificat est lu au démarrage : redémarrer après un renouvellement (arrêt gracieux).

## Configuration

Un seul fichier TOML (chemin en argument, défaut `config.toml`). Exemple commenté :
[`myproxy/config.toml.example`](myproxy/config.toml.example). Toute clé inconnue ou mal
placée fait échouer le démarrage. Durées au format `"5s"`, `"1m"`, `"15m"`.

### `[server]`

| Clé | Défaut | Rôle |
|---|---|---|
| `listen` | `"0.0.0.0:8080"` | port en clair (HTTP + CONNECT), adresse IP littérale |
| `workers` | `1` | nombre de shards (un thread + un listener `SO_REUSEPORT` chacun) ; `0` = un par CPU |
| `pin_cores` | `false` | épingle le shard *i* sur le *i*-ème CPU autorisé ; avec un shard par CPU, chaque connexion est dirigée vers le shard du CPU qui a reçu son SYN (`SO_ATTACH_REUSEPORT_CBPF`) |
| `admin_listen` | `"127.0.0.1:8181"` | `GET /health` sans auth ; `""` désactive |
| `socket_buffer_bytes` | `1048576` | `SO_SNDBUF`/`SO_RCVBUF` de chaque socket relayée ; `0` = autotuning du noyau. Une taille fixe coupe l'autotuning et est plafonnée par `net.core.rmem_max`/`wmem_max` (avertissement au démarrage) : 1 MB en vol au plus par connexion, soit ~80 Mbit/s à 100 ms de RTT. Mettre `0` pour des clients lointains |

### `[auth]`

| Clé | Défaut | Rôle |
|---|---|---|
| `api_keys` | `[]` | clés acceptées ; **vide = pas d'authentification (relais ouvert)** |
| `allow_clients` | `[]` | IP/CIDR autorisés à se connecter ; vide = tout le monde |
| `max_failures` | `5` | mauvaises clés tolérées par IP dans la fenêtre ; `0` = pas de ban |
| `failure_window` | `"1m"` | fenêtre de comptage |
| `ban_duration` | `"15m"` | durée du ban (connexions fermées à l'accept) |

Une requête **sans** identifiants (premier 407 d'un navigateur) ne compte pas comme un échec.

### `[limits]` et `[timeouts]`

| Clé | Défaut | Rôle |
|---|---|---|
| `limits.max_connections` | `10000` | connexions clientes simultanées (tous shards) ; au-delà : 503 |
| `limits.max_header_bytes` | `65536` | taille max d'un head de requête (431) ou de réponse |
| `timeouts.connect` | `"5s"` | connexion vers l'origine (504) |
| `timeouts.idle` | `"60s"` | inactivité sur une connexion ou un tunnel |
| `timeouts.request_head` | `"10s"` | délai total pour recevoir un head / un handshake SOCKS5 |

### `[http]`

| Clé | Défaut | Rôle |
|---|---|---|
| `upstream_keepalive` | `true` | réutiliser les connexions vers les origines |
| `max_idle_per_origin` | `256` | connexions inactives gardées par origine **et par shard** (plafond global 1024 par shard) |
| `max_request_body_bytes` | `0` | taille max d'un body de requête (413) ; `0` = illimité |
| `max_response_body_bytes` | `0` | taille max d'une réponse (502 si déclarée, coupure sinon) ; `0` = illimité |

### `[dns]`

| Clé | Défaut | Rôle |
|---|---|---|
| `ttl` | `"60s"` | durée du cache des résolutions ; `"0s"` désactive |
| `max_entries` | `4096` | taille du cache (vidé quand elle est dépassée) |

### `[tls]` — proxy HTTPS / HTTP/2

| Clé | Défaut | Rôle |
|---|---|---|
| `listen` | `""` | port HTTPS (ex. `"0.0.0.0:8443"`) ; vide = désactivé |
| `cert` / `key` | `""` | certificat (chaîne) et clé privée PEM |
| `handshake_timeout` | `"10s"` | durée max d'un handshake TLS |
| `http2` | `true` | proposer HTTP/2 via ALPN |
| `ktls` | `false` | chiffrement par le noyau après le handshake (module `tls` requis, vérifié au démarrage) — voir [décisions](#décisions-mesurées) |

### `[filter]` — destinations

| Clé | Défaut | Rôle |
|---|---|---|
| `block_private` | `true` | refuse loopback, réseaux privés, link-local, CGNAT, multicast, IPv6 ULA… (403). `false` pour joindre son LAN |
| `deny` | `[]` | réseaux refusés en plus (CIDR) |
| `allowed_ports` | `[]` | ports de destination autorisés (CONNECT, HTTP, SOCKS5) ; vide = tous. Ex. `[80, 443]` |

### `[socks5]`

| Clé | Défaut | Rôle |
|---|---|---|
| `listen` | `""` | port SOCKS5 (ex. `"127.0.0.1:1080"`) ; vide = désactivé. Mot de passe = une des `api_keys` |

### `[log]` — log d'accès

| Clé | Défaut | Rôle |
|---|---|---|
| `access` | `"errors"` | `off` · `errors` (refus et échecs : 403, clé fausse, ban, 4xx/5xx, réponses tronquées) · `all` (+ une ligne par requête et par tunnel) |
| `output` | `"file"` | `file` ou `stdout` (journald / `docker logs`) |
| `file` | `""` | chemin du fichier si `output = "file"` ; vide = pas de log d'accès |
| `target` | `"host"` | `host` (`host:port`) ou `url` (URL complète des requêtes `http://`) |

Format : une ligne `clé=valeur` horodatée par événement, par exemple
`2026-10-01T10:00:00.123Z client=1.2.3.4 via=http1 kind=connect target=example.com:443 status=200 up=773 down=5489 ms=43`.
L'écriture se fait dans un thread dédié et ne bloque jamais le proxy (en cas de retard,
des lignes sont perdues et comptées : `event=dropped`). Rotation : renommer le fichier
puis `kill -HUP <pid>` (`postrotate` de logrotate). Les diagnostics (`RUST_LOG`) sont
sur stderr.

## Développement

```sh
make check      # fmt + clippy (-D warnings) + tests — avant chaque commit
make test       # tests (unitaires et de bout en bout : HTTP, TLS, h2, SOCKS5, policy, logs)
make help       # toutes les cibles
```

```text
myproxy/src/
├── main.rs            démarrage, signaux (SIGTERM/SIGINT arrêt, SIGHUP log)
├── config/            types de config + parsing TOML
├── server/            shards, listeners (clair / TLS / SOCKS5), admin, arrêt gracieux
├── proxy/
│   ├── mod.rs         connexion HTTP/1 : keep-alive, auth, routage CONNECT / HTTP
│   ├── pipeline.rs    requête HTTP : pool amont, forward, réponse, uploads chunked
│   ├── h2.rs          HTTP/2 : streams, CONNECT sur stream, forward vers HTTP/1.1
│   ├── socks5.rs      SOCKS5 CONNECT
│   └── socks5_udp.rs  SOCKS5 UDP ASSOCIATE
├── relay/             transports : splice zéro-copie, copie adaptative, chunked
├── http/              parsing des heads (requête zéro-allocation, réponse)
├── stream.rs          transport client générique (TCP / TLS / kTLS)
├── tls.rs, ktls_offload.rs   rustls, offload noyau
├── policy.rs          destinations, allowlist clients, bans
├── accesslog.rs       log d'accès
├── auth.rs, dns.rs, upstream.rs
bench/                 harnais de comparaison (voir ci-dessous)
```

Documentation pour agents / contributeurs : [`AGENTS.md`](AGENTS.md).

## Benchmarks

Harnais Docker : générateur de charge maison (`bench/generator`), origine nginx, proxy
testé épinglé sur **un core**, harnais sur l'autre, ordre des tests mélangé à graine
fixe (`2004113323`), médiane de 3 runs. Proxies comparés : 3proxy, Squid, gost,
Tinyproxy (même clé, logs d'accès coupés, même type de certificat).

```sh
make bench P=myproxy A=on                 # une matrice (P = myproxy|3proxy|squid|gost|tinyproxy)
make bench-all                            # campagne complète : 5 proxies × clair / TLS / h2, puis tableaux
PROXY_TLS=1 ./bench/run-bench.sh squid on # un transport précis (PROXY_H2=1 pour HTTP/2)
NCPUS="1 2 4 8" make bench-scaling        # courbe multi-core (machine ≥ 2 × N cores)
```

### Derniers chiffres (campagne du 2026-10-03, un core, auth activée)

Débit · CPU du proxy par requête. Tous les tests sans erreur côté proxy. Protocole de
mesure M21.5 (warmup hors échantillon) : ne pas comparer aux campagnes antérieures.

**Clair (HTTP/1.1)**

| Test | **myproxy** | 3proxy | Squid | gost | Tinyproxy |
|---|---|---|---|---|---|
| 1 KB keep-alive | **14 594 rps · 44 µs** | 1 557¹ · 67 µs | 8 964 · 110 µs | 1 935 · 510 µs | 2 962 · 313 µs |
| 10 KB | **14 211 · 54 µs** | 12 343 · 74 µs | 7 292 · 136 µs | 1 850 · 521 µs | 2 601 · 353 µs |
| 1 MB | **2 453 · 224 µs** | 2 159 · 329 µs | 306 | 688 | 128 |
| 10 MB | **319** | 264 | 31 | 104 | 14 |
| Connexions courtes | **4 884 · 75 µs** | 3 403 · 230 µs | 3 795 · 212 µs | 1 916 · 507 µs | 2 996 · 306 µs |
| Tunnels CONNECT (flood) | **19,3 Gbps** | 18,4 | 13,0 | 11,6 | 1,2 |
| 25 000 tunnels (pic RAM) | **25 000 · 78 MB** | 17 154 · 380 MB | 25 000 · 896 MB | —² | 16 746 · 314 MB |

**Proxy HTTPS (TLS)**

| Test | **myproxy** | Squid | gost |
|---|---|---|---|
| 1 KB | **14 128 · 50 µs** | 7 826 · 126 µs | 1 611 · 601 µs |
| 1 MB | **1 444 · 654 µs** | 178 | 468 |
| Connexions courtes (handshake) | **2 727 · 233 µs** | 1 600 · 611 µs | 1 566 · 605 µs |
| Tunnels CONNECT (flood) | **11,4 Gbps** | 4,8 | 6,7 |
| 25 000 tunnels (pic RAM) | **25 000 · 788 MB** | 25 000 · 1 846 MB | 1 226² |

**HTTP/2**

| Test | **myproxy** | gost |
|---|---|---|
| 1 KB | **14 896 · 57 µs** | 3 904 · 254 µs |
| 1 MB | **792 · 1 190 µs** | 317 · 3 093 µs |
| Tunnels CONNECT (flood)³ | 7,4 Gbps · **0,91 s CPU/GB** | 7,6 Gbps · 1,05 s CPU/GB |
| 25 000 tunnels⁴ | **21 264 · 941 MB** | 462 |

¹ 3proxy sur 1 KB : plombé par son delayed-ACK de 40 ms (artefact connu, voir RESULTS.md).
² gost dépasse la RAM de la machine (4 GB) : conteneur tué ou tunnels refusés pendant le test.
³ Débit borné par le core du harnais (générateur + sink) : l'écart de débit (−3 %) est dans
le bruit du flood (±15 %) ; à débit égal myproxy dépense 13 % de CPU de moins par Go.
⁴ Plafond du harnais (handshakes TLS du générateur sur le core partagé, p50 1,3 s), stable
depuis la première campagne.
myproxy est premier sur toutes les lignes, sauf le débit du flood HTTP/2 (égalité dans le bruit,
CPU par Go meilleur).
Tableaux complets : [`bench/RESULTS.md`](bench/RESULTS.md) (section M21, campagne finale).

## Décisions mesurées

Chaque choix de performance est passé par la matrice à graine fixe (détails dans
`bench/RESULTS.md`) :
- **runtime Tokio `current_thread` par shard** : −11 % de CPU par requête sur les connexions
  courtes par rapport à un runtime multi-thread à un worker ;
- **pas d'io_uring** : un prototype monoio (M18, retiré du repo, commit `a2b11e3`) fait ±8 % selon la taille,
  sans gain net — sur un core, le coût est dans la pile TCP du noyau (~80 % du CPU) ;
  à retester en multi-core ;
- **kTLS en option** : +13 % sur les tunnels CONNECT et −30 % de mémoire à 25 000 tunnels,
  mais −37 % sur les gros GET `http://` et +15 % de CPU par nouvelle connexion ;
- **log d'accès par lots** : +2,9 % de CPU par requête en mode `all` (un réveil du thread
  d'écriture par ligne coûtait +26 %) ;
- **allocateur** : mimalloc testé, neutre (le chemin chaud n'alloue pas).
- **buffers socket 1 MB fixes** plutôt que l'autotuning sur le bench (LAN) : flood −56 % de CPU
  par Go, 10 MB +30 % de débit, 1 KB −10 % de CPU ; l'autotuning reste disponible pour le WAN ;
- **`TCP_DEFER_ACCEPT`** : −7 % de CPU par connexion courte (le réveil d'`accept` attend la
  requête) ; QUICKACK une fois par connexion : neutre, conservé ;
- **pipes de splice en pool par shard**, pipes réduits par le noyau refusés : au-delà de ~60
  tunnels, le budget `fs.pipe-user-pages-soft` donnait des pipes de 8 KB (flood à 128 tunnels
  10,0 → 16,9 Gbps).
