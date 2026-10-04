# e2e — black-box functional tests

Runs **the real binary** (`target/release/myproxy`), one process per scenario, with a
generated config file. The proxy is driven from the outside by real clients: HTTP/1.1,
HTTPS, HTTP/2, SOCKS5 TCP/UDP and the system's curl. On the other side are programmable
local origins. Nothing is mocked, no proxy code is imported.

```bash
make e2e                      # full suite (~2 min, ~530 scenarios)
make e2e-quick                # subset (~20 s), run by `make check`
make e2e F="socks5 policy"    # filter: substrings of the name, at least one must match

cargo run --profile e2e -p e2e -- [FILTER...] [--quick] [--jobs N] [--list] [--keep] [--bin PATH]
```

- `--list`: lists the scenarios without running them.
- `--keep`: keeps the temporary directories (config, certificates, access logs).
- `--jobs N`: scenarios run in parallel (default: number of CPUs).
- Exit code ≠ 0 when a scenario fails. On failure, the report shows the end of the
  stderr of the proxy involved.

Before running the suite, build the binary under test: `cargo build --release -p myproxy`
(done by `make e2e`).

## Automatic checks

These checks apply to every scenario that ends with `Proxy::finish()`:

- **leak**: no connection closed by its client may stay open on the proxy side (socket
  in `CLOSE_WAIT`);
- **shutdown**: SIGTERM must stop the process within 8 s, with exit code 0, without a
  panic and with the `shutdown complete` line.

The real ports are read from the `myproxy listening` startup line, since every listener
is on `127.0.0.1:0`: no port races, safe parallel execution. The harness's own listeners
have a deep accept queue (4096), so 1,000 simultaneous tunnels never overflow a target.

A **SKIP** has a stated reason and is not a failure: kTLS missing from the kernel, curl
without `--proxy-http2` (curl < 8.1), 127.0.0.2 unavailable.

## Layout

```
src/
  main.rs        CLI + report
  runner.rs      registry, parallel execution, timeouts, SKIP, report per suite
  harness/       infrastructure, no assertions
    proxy.rs     Config (test TOML → file), Proxy (spawn, ports, /proc: fds, RSS,
                 sockets, signals, finish)
    origin.rs    programmable HTTP/1.1 origin (the path selects the behavior)
    targets.rs   TCP echo, server that speaks first, TLS echo, HTTPS origin, UDP echo
                 (127.0.0.1 and ::1 on the same port)
    payload.rs   deterministic payloads (byte i = f(seed, i)), checked byte for byte
    certs.rs     localhost certificate (rcgen), rustls client/server configs
  clients/       http1.rs (plain + TLS), h2.rs (requests + CONNECT tunnels as
                 AsyncRead/Write), socks5.rs (CONNECT, UDP ASSOCIATE), curl.rs
  suites/        one file per feature = one report category
```

## Coverage (suites)

| Suite | Content |
|---|---|
| `matrix` | 13 configs × transports (`h1`, `tls`, `tls12`, `h2`, `socks5`) × operations: GET with `cl` / `chunked` / `chunked-small` / `eof` framings, `cl` / `chunked` / length-less (h2) uploads, CONNECT (echo and end-to-end TLS), UDP. Sizes at the engine boundaries: 0, 1, 8,191, 8,193, 64 KB ± 1, 128 KB ± 1, 1 MB, 10 MB, and 64 MB for the default config. Configs: `default`, `workers2-pin`, `workers0`, `keepalive-off`, `sockbuf-autotune`, `ktls`, `open-relay`, `dns-names`, `dns-nocache`, `log-all-file`, `log-all-stdout`, `policy`, `limits-generous` |
| `http` | upstream pool reused or not, `Connection: close`, HTTP/1.0, HEAD/204/304, status codes, 1xx, chunked trailers, slow origin, GET retry on a dead pooled socket, POST never replayed, origin that resets / stays silent (504) / refuses (502), DNS failure, connect timeout, IPv6, host name, origin-form, header and request-line rewriting, methods, large response head, 50 headers, pipelining, `Expect: 100-continue` |
| `connect` | server that speaks first, target that closes first, refusal, DNS failure, IPv6 target, on all 4 transports; early bytes; 1,000 simultaneous tunnels; 200 h2 CONNECT streams on one connection; idle timeout; tunnel to the proxy itself |
| `h2` | multiplexing of mixed sizes and framings, ALPN with `http2 = false`, 407, HEAD/204/304/1xx, upstream errors, RST_STREAM mid-download, interleaved tunnel and requests |
| `socks5` | method without credentials refused, wrong key, other user + second key, open relay, target by name, BIND refused, SOCKS4 closed, UDP IPv4 / name / IPv6, UDP filtered by the policy, foreign source ignored, association ends with the TCP control connection, 500 datagrams |
| `uploads` | chunked extensions and trailers, single 12 MB chunk, one-byte chunks, slow sender, chunked in HTTP/1.0 (400), CL + TE (400), duplicate CL (400), invalid chunk size (400), bytes after the final chunk (close) |
| `limits` | `max_header_bytes` (431), request body budgets (413 for CL, chunked and h2), response budgets (502 when declared, cut otherwise), global `max_connections` across 2 shards, HTTP and SOCKS5 slowloris, TLS handshake timeout, generous limits that cut nothing |
| `security` | missing / wrong / valid auth on all 4 transports, malformed credentials, key containing `:`, bans then expiry (407s without credentials do not count, SOCKS5 failures do), `allow_clients` (closed at accept, filtering by source address), `block_private` (loopback, RFC 1918, cloud metadata, CGNAT, ULA, link-local, rebinding by name), `deny`, `allowed_ports`, open relay, policy checked before connecting |
| `robustness` | client cutting a download / an upload / tunnels, slow reader (backpressure: bounded RSS), malformed requests, half requests, garbage on the TLS / SOCKS5 / h2 ports, 2,000 successive connections without fd growth, huge request line |
| `process` | shipped example config, invalid configs (exit code ≠ 0 + message), busy port, SIGTERM (in-flight transfer completed, no more accepts, exit in time with an open tunnel), SIGHUP without a log, admin disabled / 404 |
| `logs` | `all` (one line per request / tunnel / h2 stream / UDP association), `errors` (refusals only), `target = url`, stdout output, `off`, SIGHUP reopen after rename, aggregated refusals at accept |
| `curl` | GET through the HTTP proxy, `-p` tunnel, HTTPS proxy (verified certificate), HTTP/2 proxy (when curl ≥ 8.1), `socks5h`, end-to-end HTTPS through CONNECT, POST and chunked uploads, 407 |

## Adding a scenario

In the feature's file (`suites/<feature>.rs`), add an element to `scenarios()`:

```rust
scenario("http/my-case", |env| async move {
    let p = Proxy::start(Config::new().set("http", "upstream_keepalive", "false")).await?;
    let mut c = Client::plain(p.addr).await?;
    let r = c.roundtrip(&get(&env.origin.url("/data/1000")), false).await?;
    r.expect(200)?;
    payload::check(1000, 1000, &r.body)?;
    drop(c);
    p.finish().await // leak check + clean shutdown
})
.quick(), // when the case must run in `make check`
```

- `Config::new()`: listeners on free ports, auth on (`secret1`, `secret2`) and
  `block_private = false` (every target is local). Values passed to `set` are raw TOML.
- A case that cannot run on the machine returns `Err(skip("reason"))`.
- The name is hierarchical: the first segment is the report category.
- A bug found → first a scenario that reproduces it (it must fail), then the fix.
