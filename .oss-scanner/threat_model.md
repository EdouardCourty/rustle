# Rustle threat model

## What this project does

Rustle is a forward HTTP/HTTPS proxy written in Rust (Tokio, one OS thread per shard, `SO_REUSEPORT`).
Clients authenticate with an API key and ask it to relay traffic to destinations of their choice. Its main
job is therefore to enforce a policy (who may connect, where they may connect to) on traffic it does not
control. Scope of this enrollment: the `rustle/` crate on `main`. Out of scope: `benchmarks/` (benchmark
harness and competitor configurations), `e2e/` (test harness), `install.sh` and `.github/` (release tooling).

Listeners (each is off unless configured, except the plain one):
- plain TCP: HTTP/1.1 proxy requests, `CONNECT` tunnels, WebSocket upgrades;
- TLS (`[tls]`): HTTP/1.1 or HTTP/2, chosen by ALPN; optional opt-in kernel TLS on Linux;
- SOCKS5 (`[socks5]`): `CONNECT` and `UDP ASSOCIATE`, password = API key;
- admin (`/health` only, loopback by default) and a Unix control socket (`reload`, mode 0600).

## Who can attack, and from where (attacker positions)

Treat as attacker-controlled:
- **Any network peer of a listener**, before authentication: TCP and TLS handshakes, request heads, SOCKS5
  greetings, UDP datagrams, slow or half-closed connections. Closing at accept applies only when
  `auth.allow_clients` or a ban is configured.
- **An authenticated client** (holds a valid API key): request bodies, headers, h2 frames and flow control,
  chosen destinations (host, port, resolved address), chunked framing, WebSocket frames, SOCKS5 UDP payloads.
  Authenticated clients are not trusted to bypass the destination policy or to affect other clients.
- **An origin server** the client asked Rustle to contact: response heads, framing (Content-Length, chunked,
  EOF-delimited), 1xx and `101` responses, early close, slow bodies. Origins are chosen by the client, so a
  malicious origin is a realistic peer.
- **DNS answers** for the requested names (rebinding, multiple A records, IPv4-mapped IPv6).
- **Another client** sharing the proxy: one client must not read or influence another client's connection
  (pool reuse, shard state, ban list, access log).

Trusted: the operator and the TOML config, the TLS key and certificate, the `api_keys`, the control socket
(0600, same user), the admin listener, the host and its kernel. A finding that needs control of one of these
is out of scope unless it escalates beyond what that access already allows.

## Security goals (what a finding should break)

1. **Destination policy.** `filter.block_private` (default on), `filter.deny` and `filter.allowed_ports` hold
   for every way to reach an upstream: `CONNECT`, absolute-URI HTTP, h2 (including h2 `CONNECT`), SOCKS5
   `CONNECT`, SOCKS5 UDP, WebSocket upgrade. The check applies to the resolved address, at connect time
   (DNS rebinding, IPv4-mapped IPv6, names resolving to private ranges). A bypass that reaches a private or
   denied address is an SSRF.
2. **Client admission.** `auth.api_keys` (Basic auth, SOCKS5 password), `auth.allow_clients` and the per-IP
   ban limiter (`max_failures` 5, `failure_window` 1 min, `ban_duration` 15 min by default) cannot be
   bypassed: no relay without a valid key when keys are configured, no ban evasion, no ban-list poisoning
   of other IPs, no key disclosure through timing, logs or error messages.
3. **HTTP message integrity.** The proxy does not create request or response smuggling or desynchronization:
   conflicting or duplicate `Content-Length`, `Transfer-Encoding`, chunked framing (relayed verbatim in both
   directions), pipelined requests (the first is answered, then the connection closes), header folding, bare
   CR or LF, request target and `Host` confusion, and pooled upstream connections (a response is never
   delivered to the wrong client; the pool accepts only complete, keep-alive, exactly framed responses).
4. **Memory and concurrency safety.** `unsafe` is confined to libc and fd code: `relay/splice.rs` (pipes,
   `splice`, `dup`), `server/mod.rs`, `server/socket.rs` and `server/cpu.rs` (setsockopt, CPU affinity,
   `malloc_trim`), `ktls_offload.rs` (TCP_ULP, SOL_TLS, `sendmsg`), `stream.rs` (`ReadAhead`, `set_len` after
   a read), `http/request.rs::span_of` (`offset_from`), plus trivial libc calls in `sys.rs` and
   `cmd/service.rs`. `RequestHead` holds spans into a reused buffer: a path where that buffer is cleared or
   reallocated between parse and use is a bug. File descriptors closed and reused by another connection,
   data races and panics reachable from the network matter.
5. **Availability within configured limits.** A peer must not take down a shard or the process, leak file
   descriptors or sockets (CLOSE_WAIT), exceed `limits.max_connections` (10000), `limits.max_header_bytes`
   (64 KiB), a configured `http.max_request_body_bytes` or `max_response_body_bytes`, grow the pool or DNS
   cache past their caps, or hold a tunnel past `timeouts.idle` (60 s), `timeouts.request_head` (10 s),
   `timeouts.connect` (5 s) or `tls.handshake_timeout` (10 s). A panic ends a whole shard, so a
   network-reachable panic is more than a single-request failure.
6. **Reload and control.** `rustle reload` and SIGHUP keep the old state on any invalid config; open
   connections keep the snapshot they were accepted under.

## Library guarantee versus operator choice

Rustle offers protections; an operator can loosen them. A finding weighs more when it works against the
defaults than when it needs a loosened setting. Not a finding on its own: `auth.api_keys = []` (documented
open relay, flagged by `rustle check`), `filter.block_private = false`, empty `allowed_ports`, no body limits (the
default is unlimited), `server.listen` on a public interface, `tls.ktls = true`. An SSRF report must bypass a
protection Rustle actually offers. Say which settings the reproducer changes and why they are realistic.

## Components that matter most

`http/request.rs`, `http/response.rs`, `proxy/pipeline.rs`, `relay/chunked.rs`, `relay/splice.rs`,
`relay/adaptive.rs`, `proxy/h2.rs`, `proxy/socks5.rs`, `proxy/socks5_udp.rs`, `proxy/upgrade.rs`, `policy.rs`,
`auth.rs`, `upstream.rs`, `dns.rs`. In scope but less central: `tls.rs`, `ktls_offload.rs` (opt-in, Linux),
`accesslog.rs`, `server/`, `cmd/`.

## Dependencies

A flaw wholly inside a dependency (tokio, rustls, ring, h2, httparse) belongs to that project. Report it here
only if Rustle's use of it makes it reachable, and show the path from a listener.

## How to exercise it

The image has everything prebuilt under `/src` and no network. Loopback only; reproducers must not need
Internet.
- `cargo test -p rustle`: unit tests and in-process end-to-end tests (`rustle/src/server/tests/`).
- `cargo run --profile e2e -p e2e -- --list` shows the black-box scenarios, `-- --quick` runs a subset,
  `-- <filter>` runs matching ones. They start `target/release/rustle` with a generated config and use raw
  HTTP/1.1, h2, SOCKS5 and curl clients plus programmable origins (`e2e/src/harness/`). A reproducer as a new
  scenario in `e2e/src/suites/<feature>.rs` is ideal.
- `target/release/rustle config.toml` runs the proxy (see `rustle/config.toml.example`); `rustle check
  config.toml` validates a config. The destination policy blocks loopback by default, so a reproducer that
  targets a local origin must set `filter.block_private = false` and say so.
- Prefer raw bytes with explicit chunk boundaries (a small script or `nc`) over high-level clients, which
  normalize malformed input.

## How severity is rated

- **Critical**: remote code execution or memory corruption reachable by an unauthenticated peer; auth
  bypass that gives an open relay on a deployment with keys configured.
- **High**: bypass of the destination policy at defaults (SSRF to private, loopback or denied addresses);
  request or response smuggling with cross-client impact; auth or ban bypass; memory unsafety reachable only
  with authentication; a remote crash of the whole process or of every shard.
- **Medium**: remote crash of one shard; exhaustion of memory, descriptors or connections beyond a
  configured limit; descriptor leaks; limited disclosure of another client's data.
- **Low**: hardening gaps, misleading logs, issues needing a loosened non-default setting or operator access.
- Without a reproducer (config, bytes sent, observed result) a finding is not rated above Low.

## Anything to leave alone

- Open relay when `auth.api_keys` is empty (documented, flagged by `rustle check`); the API key travelling in
  clear text on the plain and SOCKS5 ports (documented: expose those only on a trusted interface, use the TLS
  port over the Internet); behavior needing write access to the config file, the TLS key or the control
  socket.
- Throughput, latency or CPU-per-request observations; benchmark numbers.
- Known gap: a duplicate `Content-Length` in an origin response is not checked (origins are assumed to be
  chosen by the client and are not trusted for other clients); report it only with cross-client impact.
- kTLS performance (opt-in, Linux; correctness bugs are in scope).
- Style findings, unreachable code, theoretical issues without a path from a listener.
