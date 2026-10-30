# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0] - 2026-10-30

Initial release.

### Added

- HTTP/1.1 forward proxy and `CONNECT` tunnels, with keep-alive connection pooling to origins.
- HTTPS proxy (TLS via rustls) and HTTP/2 over TLS, negotiated by ALPN.
- SOCKS5 over TCP and UDP (`UDP ASSOCIATE`).
- WebSocket upgrades.
- Zero-copy relay with `splice` on Linux; opt-in kernel TLS (kTLS).
- Shared-nothing multi-core architecture: one shard per core, optional core pinning.
- Basic authentication with API keys, per-IP bans after repeated failures, client IP allowlist.
- Destination policy: allowed ports, private and denied ranges blocked by default, DNS rebinding protection.
- Request and response size limits, connection limits, and timeouts including anti-slowloris protection.
- Access log with `off`, `errors` and `all` levels, reopened on `SIGHUP`.
- Runtime configuration reload (`rustle reload` or `SIGHUP`) and a `/health` endpoint.
- `rustle` command line: `init`, `check`, `status`, `reload`, `service` (systemd and launchd), `update`, `uninstall`.
- `curl | sh` installer with SHA-256 verification; Linux (x86_64, aarch64) and macOS (Apple Silicon, Intel) builds.
- Black-box end-to-end test suite and a reproducible benchmark harness; results are in `benchmarks/documentation/RESULTS.md`.

[Unreleased]: https://github.com/EdouardCourty/rustle/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/EdouardCourty/rustle/releases/tag/v1.0.0
