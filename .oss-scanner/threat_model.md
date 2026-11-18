# Rustle threat model

## What this project does and where untrusted input enters

Rustle is a forward HTTP/HTTPS proxy in Rust (Tokio, shared-nothing shards). Scope: the `rustle/` crate
on `main`. Out of scope: `benchmarks/` (benchmark harness and competitor configs), `e2e/` (test harness),
`install.sh` and `.github/` (release tooling).

Untrusted input, in order of importance:

- **Clients** of the proxy, on three listeners: plain TCP (HTTP/1.1, `CONNECT`, WebSocket upgrade), TLS
  (HTTP/1.1 or HTTP/2 chosen by ALPN) and SOCKS5 (`CONNECT` and `UDP ASSOCIATE`). A client can send any
  bytes before and after authentication: request heads, header sets, chunked and Content-Length bodies,
  h2 frames and flow-control updates, SOCKS5 handshakes, UDP datagrams, TLS handshakes, slow or half-closed
  connections.
- **Origin servers** the proxy connects to on behalf of clients: response heads, framing (Content-Length,
  chunked, EOF-delimited), 1xx responses, `101` upgrades. Origins are chosen by the client, so a malicious
  origin is a realistic peer.
- **DNS answers** for the requested host names.

Trusted: the TOML config file and whoever edits it, the TLS key, the `api_keys`, the control socket (mode
0600, same user), the admin `/health` listener (loopback by default). A finding that needs control of one
of these is out of scope unless it escalates beyond what that access already allows.

## Security goals (what a finding should break)

1. **Destination policy.** `filter.block_private` (default on), `filter.deny` and `filter.allowed_ports` must
   hold for every way of reaching an upstream: `CONNECT`, absolute-URI HTTP, h2 (including h2 `CONNECT`),
   SOCKS5 `CONNECT`, SOCKS5 UDP, WebSocket upgrade. The check applies to the resolved address (DNS rebinding,
   IPv4-mapped IPv6, redirects through names that resolve to private ranges). A bypass that reaches a
   private or denied address is an SSRF.
2. **Client admission.** `auth.api_keys` (Basic auth / SOCKS5 password), `auth.allow_clients` (closed at
   accept) and the per-IP ban limiter (`max_failures`, `failure_window`, `ban_duration`) must not be
   bypassable: no unauthenticated relay when keys are configured, no ban evasion or ban-list poisoning, no
   key leak through timing, logs or error messages. Auth off (`api_keys = []`) is a documented open relay and
   not a finding.
3. **HTTP message integrity.** The proxy must not create request or response smuggling or desynchronization:
   conflicting or duplicate `Content-Length`, `Transfer-Encoding` handling, chunked framing (relayed
   verbatim in both directions), pipelined requests (the first is answered, then the connection closes),
   header folding, bare CR/LF, request-target and `Host` confusion between the proxy and the origin, pooled
   upstream connection reuse (a response must never be delivered to the wrong client; the pool accepts only
   complete, keep-alive, exactly framed responses). Known gap: duplicate `Content-Length` in *origin*
   responses is not checked; report it only with a concrete cross-client impact.
4. **Memory and concurrency safety.** `unsafe` is confined to libc and fd code: `relay/splice.rs` (pipes,
   `splice`, `dup`), `server/mod.rs`, `server/socket.rs` and `server/cpu.rs` (setsockopt, CPU affinity,
   `malloc_trim`), `ktls_offload.rs` (TCP_ULP, SOL_TLS, `sendmsg`), `stream.rs` (`ReadAhead`, `set_len` after
   read), `http/request.rs::span_of` (`offset_from`), plus trivial libc calls in `sys.rs` and `cmd/service.rs`. `RequestHead` holds spans into a reused buffer: any path where the buffer is cleared or
   reallocated between parse and use is a bug. Any memory-safety violation, use-after-free of a spans or fd
   (closed fd reused by another connection), data race or panic reachable from the network matters.
5. **Availability.** A client must not be able to take down a shard or the process, leak file descriptors or
   sockets (CLOSE_WAIT), exhaust memory beyond the configured limits (`limits.max_connections`,
   `max_header_bytes`, `http.max_request_body_bytes`, `max_response_body_bytes`, pool caps, DNS cache size),
   or hang a tunnel forever (`timeouts.idle`, `timeouts.request_head`, `tls.handshake_timeout`). A panic
   kills a whole shard (all its connections), so a network-reachable panic is more than a single-request
   failure.
6. **Reload and control.** `rustle reload` / SIGHUP must keep the old state on any invalid config, and
   open connections keep the snapshot they were accepted under.

## Components that matter most

`http/request.rs`, `http/response.rs`, `proxy/pipeline.rs`, `relay/chunked.rs`, `relay/splice.rs`,
`relay/adaptive.rs`, `proxy/h2.rs`, `proxy/socks5.rs`, `proxy/socks5_udp.rs`, `proxy/upgrade.rs`, `policy.rs`,
`auth.rs`, `upstream.rs`, `dns.rs`. Less important but in scope: `tls.rs`, `ktls_offload.rs` (opt-in, Linux),
`accesslog.rs`, `server/`, `cmd/`.

## How to exercise it

- `cargo test -p rustle`: unit tests and in-process end-to-end tests (`rustle/src/server/tests/`).
- `e2e/`: black-box scenarios against the release binary (`cargo build --release -p rustle`, then
  `cargo run --profile e2e -p e2e -- [--quick] [filter]`; `--list` shows the scenarios). Clients available:
  raw HTTP/1.1, h2, SOCKS5, curl. Programmable origins and targets are in `e2e/src/harness/`. A reproducer
  as a new scenario in `e2e/src/suites/<feature>.rs` is ideal.
- A proxy config is a TOML file passed as the first argument: `rustle config.toml` (see
  `rustle/config.toml.example`). Policy defaults to blocking private ranges, so a reproducer that targets a
  local origin must set `filter.block_private = false` and say so.
- `rustle check <config>` validates a config without starting.

## How severity is rated

- **Critical**: remote code execution, memory corruption reachable by an unauthenticated network peer,
  authentication bypass that gives an open relay on a configured-auth deployment.
- **High**: bypass of destination policy (SSRF to private, loopback or denied addresses), request or response
  smuggling with cross-client impact, auth or ban bypass by an authenticated or low-privilege peer, memory
  unsafety reachable only with authentication.
- **Medium**: remote crash of a shard or the process, resource exhaustion beyond a configured limit,
  descriptor leaks, information disclosure of another client's data limited in scope.
- **Low**: hardening gaps, misleading logs, issues needing operator-level access or unusual non-default
  configuration.
- A finding is worth reporting only with a reproducer (config, bytes sent, observed result). State the
  attacker's starting position (unauthenticated, valid key, malicious origin).

## Anything to leave alone

- Open relay when `auth.api_keys` is empty (documented, warned at startup).
- Behavior that needs write access to the config file, the TLS key or the control socket.
- Throughput, latency or CPU-per-request observations; benchmark numbers.
- kTLS (`tls.ktls`, opt-in, Linux) performance; kTLS correctness bugs are in scope.
- Clippy-style or style findings; unreachable code; theoretical issues without a path from the network.
- Dependencies' own advisories unless Rustle's use of them is exploitable (say how).
