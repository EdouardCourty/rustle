# e2e — tests fonctionnels boîte noire

Lance **le vrai binaire** (`target/release/myproxy`), un process par scénario, avec un
fichier de config généré. Le proxy est piloté de l'extérieur par de vrais clients :
HTTP/1.1, HTTPS, HTTP/2, SOCKS5 TCP/UDP et le curl du système. En face, il y a des
origines locales programmables. Rien n'est mocké, aucun code du proxy n'est importé.

```bash
make e2e                      # suite complète (~2 min, ~530 scénarios)
make e2e-quick                # sous-ensemble (~20 s), lancé par `make check`
make e2e F="socks5 policy"    # filtre : sous-chaînes du nom, au moins une doit matcher

cargo run --profile e2e -p e2e -- [FILTRE...] [--quick] [--jobs N] [--list] [--keep] [--bin CHEMIN]
```

- `--list` : liste les scénarios sans les lancer.
- `--keep` : garde les répertoires temporaires (config, certificats, logs d'accès).
- `--jobs N` : nombre de scénarios en parallèle (défaut : nombre de CPU).
- Code de sortie ≠ 0 si un scénario échoue. En cas d'échec, le rapport affiche la fin
  du stderr du proxy concerné.

Avant de lancer la suite, construire le binaire testé : `cargo build --release -p myproxy`
(fait par `make e2e`).

## Contrôles automatiques

Ces contrôles s'appliquent à chaque scénario qui termine par `Proxy::finish()` :

- **fuite** : aucune connexion fermée par son client ne doit rester ouverte côté
  proxy (socket en `CLOSE_WAIT`) ;
- **arrêt** : SIGTERM doit arrêter le process en moins de 8 s, avec code 0, sans
  panique et avec la ligne `shutdown complete`.

Les ports réels sont lus sur la ligne de démarrage `myproxy listening`, puisque tous les
listeners sont sur `127.0.0.1:0` : pas de course sur les ports, exécution parallèle
sûre.

Un **SKIP** est motivé et n'est pas un échec : kTLS absent du noyau, curl sans
`--proxy-http2` (curl < 8.1), 127.0.0.2 non disponible.

## Rangement

```
src/
  main.rs        CLI + rapport
  runner.rs      registre, exécution parallèle, timeouts, SKIP, rapport par suite
  harness/       infrastructure, aucune assertion
    proxy.rs     Config (TOML de test → fichier), Proxy (spawn, ports, /proc : fds,
                 RSS, sockets, signaux, finish)
    origin.rs    origine HTTP/1.1 programmable (le chemin choisit le comportement)
    targets.rs   écho TCP, serveur qui parle en premier, écho TLS, origine HTTPS,
                 écho UDP (127.0.0.1 et ::1 sur le même port)
    payload.rs   payloads déterministes (octet i = f(seed, i)), vérifiés octet par octet
    certs.rs     certificat localhost (rcgen), configs rustls client/serveur
  clients/       http1.rs (clair + TLS), h2.rs (requêtes + tunnels CONNECT en
                 AsyncRead/Write), socks5.rs (CONNECT, UDP ASSOCIATE), curl.rs
  suites/        un fichier par feature = une catégorie du rapport
```

## Couverture (suites)

| Suite | Contenu |
|---|---|
| `matrix` | 13 configs × transports (`h1`, `tls`, `tls12`, `h2`, `socks5`) × opérations : GET avec framings `cl` / `chunked` / `chunked-small` / `eof`, uploads `cl` / `chunked` / sans longueur (h2), CONNECT (écho et TLS de bout en bout), UDP. Tailles aux frontières du moteur : 0, 1, 8 191, 8 193, 64 KB ± 1, 128 KB ± 1, 1 MB, 10 MB, et 64 MB pour la config par défaut. Configs : `default`, `workers2-pin`, `workers0`, `keepalive-off`, `sockbuf-autotune`, `ktls`, `open-relay`, `dns-names`, `dns-nocache`, `log-all-file`, `log-all-stdout`, `policy`, `limits-generous` |
| `http` | pool amont réutilisé ou non, `Connection: close`, HTTP/1.0, HEAD/204/304, codes de statut, 1xx, trailers chunked, origine lente, retry GET sur socket poolée morte, POST jamais rejoué, origine qui reset / muette (504) / refuse (502), DNS en échec, timeout de connexion, IPv6, nom d'hôte, origin-form, réécriture des headers et de la ligne de requête, méthodes, gros head de réponse, 50 headers, pipelining, `Expect: 100-continue` |
| `connect` | serveur qui parle en premier, cible qui ferme d'abord, refus, DNS en échec, cible IPv6, sur les 4 transports ; early bytes ; 1 000 tunnels simultanés ; 200 streams h2 CONNECT sur une connexion ; timeout idle ; tunnel vers le proxy lui-même |
| `h2` | multiplexage de tailles et framings mêlés, ALPN avec `http2 = false`, 407, HEAD/204/304/1xx, erreurs amont, RST_STREAM en plein download, tunnel et requêtes entrelacés |
| `socks5` | méthode sans credentials refusée, mauvaise clé, autre user + 2e clé, open relay, cible par nom, BIND refusé, SOCKS4 fermé, UDP IPv4 / nom / IPv6, UDP filtré par la policy, source étrangère ignorée, fin de l'association avec le contrôle TCP, 500 datagrammes |
| `uploads` | extensions et trailers chunked, chunk unique de 12 MB, chunks d'un octet, envoi lent, chunked en HTTP/1.0 (400), CL + TE (400), CL dupliqué (400), taille de chunk invalide (400), octets après le chunk final (fermeture) |
| `limits` | `max_header_bytes` (431), budgets de body requête (413 en CL, chunked et h2), budgets de réponse (502 si déclaré, coupure sinon), `max_connections` global sur 2 shards, slowloris HTTP et SOCKS5, timeout de handshake TLS, limites larges qui ne coupent rien |
| `security` | auth absente / fausse / valide sur les 4 transports, credentials malformés, clé contenant `:`, bans puis expiration (les 407 sans credentials ne comptent pas, les échecs SOCKS5 comptent), `allow_clients` (fermeture à l'accept, filtrage par adresse source), `block_private` (loopback, RFC 1918, métadonnées cloud, CGNAT, ULA, link-local, rebinding par nom), `deny`, `allowed_ports`, open relay, policy vérifiée avant le connect |
| `robustness` | client qui coupe un download / un upload / des tunnels, lecteur lent (backpressure : RSS bornée), requêtes malformées, demi-requêtes, garbage sur les ports TLS / SOCKS5 / h2, 2 000 connexions successives sans croissance de fds, ligne de requête énorme |
| `process` | config d'exemple livrée, configs invalides (code ≠ 0 + message), port occupé, SIGTERM (transfert en cours terminé, plus d'accept, sortie à temps avec un tunnel ouvert), SIGHUP sans log, admin désactivé / 404 |
| `logs` | `all` (une ligne par requête / tunnel / stream h2 / association UDP), `errors` (refus seulement), `target = url`, sortie stdout, `off`, réouverture SIGHUP après rename, refus à l'accept agrégés |
| `curl` | GET via proxy HTTP, tunnel `-p`, proxy HTTPS (certificat vérifié), proxy HTTP/2 (si curl ≥ 8.1), `socks5h`, HTTPS de bout en bout à travers CONNECT, uploads POST et chunked, 407 |

## Ajouter un scénario

Dans le fichier de la feature (`suites/<feature>.rs`), ajouter un élément à `scenarios()` :

```rust
scenario("http/mon-cas", |env| async move {
    let p = Proxy::start(Config::new().set("http", "upstream_keepalive", "false")).await?;
    let mut c = Client::plain(p.addr).await?;
    let r = c.roundtrip(&get(&env.origin.url("/data/1000")), false).await?;
    r.expect(200)?;
    payload::check(1000, 1000, &r.body)?;
    drop(c);
    p.finish().await // contrôle de fuite + arrêt propre
})
.quick(), // si le cas doit tourner dans `make check`
```

- `Config::new()` : listeners sur ports libres, auth on (`secret1`, `secret2`) et
  `block_private = false` (toutes les cibles sont locales). Les valeurs passées à
  `set` sont du TOML brut.
- Un cas qui ne peut pas tourner sur la machine retourne `Err(skip("raison"))`.
- Le nom est hiérarchique : le premier segment donne la catégorie du rapport.
