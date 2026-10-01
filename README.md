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
| `pin_cores` | `false` | épingle le shard *i* sur le *i*-ème CPU autorisé |
| `admin_listen` | `"127.0.0.1:8181"` | `GET /health` sans auth ; `""` désactive |

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
| `max_idle_per_origin` | `32` | connexions inactives gardées par origine **et par shard** (plafond global 1024 par shard) |
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

### Derniers chiffres (campagne du 2026-10-01, un core, auth activée)

Débit · CPU du proxy par requête. Tous les tests sans erreur.

**Clair (HTTP/1.1)**

| Test | **myproxy** | 3proxy | Squid | gost | Tinyproxy |
|---|---|---|---|---|---|
| 1 KB keep-alive | **14 332 rps · 49 µs** | 1 558¹ · 74 µs | 8 841 · 124 µs | 1 930 · 561 µs | 2 627 · 366 µs |
| 10 KB | **14 024 · 61 µs** | 13 207 · 79 µs | 7 174 · 152 µs | 1 864 · 575 µs | 2 606 · 396 µs |
| 1 MB | **2 622 · 336 µs** | 2 437 · 369 µs | 306 | 703 | 130 |
| 10 MB | **347** | 279 | 32 | 104 | 14 |
| Connexions courtes | **4 822 · 87 µs** | 3 345 · 261 µs | 3 775 · 237 µs | 1 927 · 567 µs | 2 802 · 356 µs |
| Tunnels CONNECT (flood) | **19,0 Gbps** | 17,7 | 12,9 | 11,6 | 1,2 |
| 25 000 tunnels (pic RAM) | **25 000 · 125 MB** | 24 959 · 385 MB | 25 000 · 875 MB | 18 656² | 16 765 · 314 MB |

**Proxy HTTPS (TLS)**

| Test | **myproxy** | Squid | gost |
|---|---|---|---|
| 1 KB | **14 018 · 57 µs** | 7 837 · 141 µs | 1 566 · 676 µs |
| 1 MB | **1 297** | 182 | 464 |
| Connexions courtes (handshake) | **2 722 · 278 µs** | 1 618 · 669 µs | 1 603 · 661 µs |
| Tunnels CONNECT (flood) | **9,3 Gbps** | 4,8 | 6,6 |
| 25 000 tunnels (pic RAM) | **25 000 · 802 MB** | 25 000 · 2 028 MB | 0² |

**HTTP/2**

| Test | **myproxy** | gost |
|---|---|---|
| 1 KB | **13 028 · 70 µs** | 3 922 · 281 µs |
| 1 MB | **739** | 324 |
| Tunnels CONNECT (flood) | 6,6 Gbps | **7,7 Gbps** |

¹ 3proxy sur 1 KB : plombé par son delayed-ACK de 40 ms (artefact connu, voir RESULTS.md).
² gost dépasse la RAM de la machine (4 GB) : conteneur tué pendant le test.
myproxy est premier partout sauf sur les tunnels en HTTP/2 face à gost.
Tableaux complets : [`bench/RESULTS.md`](bench/RESULTS.md) (section M19).

## Décisions mesurées

Chaque choix de performance est passé par la matrice à graine fixe (détails dans
`bench/RESULTS.md`) :
- **runtime Tokio `current_thread` par shard** : −11 % de CPU par requête sur les connexions
  courtes par rapport à un runtime multi-thread à un worker ;
- **pas d'io_uring** : un prototype (`bench/uring-proto`, monoio) fait ±8 % selon la taille,
  sans gain net — sur un core, le coût est dans la pile TCP du noyau (~80 % du CPU) ;
  à retester en multi-core ;
- **kTLS en option** : +13 % sur les tunnels CONNECT et −30 % de mémoire à 25 000 tunnels,
  mais −37 % sur les gros GET `http://` et +15 % de CPU par nouvelle connexion ;
- **log d'accès par lots** : +2,9 % de CPU par requête en mode `all` (un réveil du thread
  d'écriture par ligne coûtait +26 %) ;
- **allocateur** : mimalloc testé, neutre (le chemin chaud n'alloue pas).
