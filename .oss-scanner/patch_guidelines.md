# How we would like patches made

## Size and scope of a fix
- Fix the root cause as narrowly as practical. If the same faulty pattern exists in a shared helper or on
  another path (plain, TLS, h2, SOCKS5, WebSocket), fix every instance with that root cause, not only the
  reproducer's call site.
- No refactoring, renames or reformatting of code the fix does not need to touch.
- A refused request keeps a static error response (400, 403, 413, 431, 502, 504); do not turn a bug into a
  silent connection drop.
- Do not edit `benchmarks/` data, `benchmarks/documentation/RESULTS.md` or the README benchmark tables:
  numbers there are generated from measured campaigns.
- Keep the hot path allocation-free: reuse `buf`, `resp_buf`, `rewrite_buf` and `origin_key`; no per-request
  `setsockopt`; do not invalidate `RequestHead` spans (no `buf.clear()` or reallocation between parse and use).
- Do not widen request retry (idempotent method with a fully buffered body only) and do not change the pool
  acceptance rules (complete, keep-alive, framed, no over-read).
- Stay inside the existing `unsafe` boundaries; any new `unsafe` block needs a `SAFETY` comment.

## Code style
- Follow the surrounding code and the edition set in `Cargo.toml`. `cargo fmt` and `cargo clippy -D warnings`
  must pass (`make check` runs both). Few comments: only where the code is not obvious, for example an
  invariant or why an order of operations matters. English only. No history or milestone labels in comments.
- No new dependency.
- Errors on the request path stay static responses (`&[u8]`) with I/O errors mostly ignored (`let _ =`);
  `anyhow` only at startup.

## Tests
- Add the failing test first: a scenario in `e2e/src/suites/<feature>.rs` (one expected behavior, one
  scenario) or an in-process test under `rustle/src/server/tests/`. Unit tests stay inline at the bottom of
  the file in `#[cfg(test)] mod tests`.
- A new or changed config key is classified as reloadable or restart-only and edited in
  `documentation/config.json`.

## Branch and commit message
- Against `main`. Subject only, no body, in the form `type(scope): description` with type among
  feat|fix|perf|refactor|docs|test|chore|ci|build; lowercase, imperative, no trailing period, about 72
  characters. No co-author or attribution trailer.
- Add one line under `## [Unreleased]` in `CHANGELOG.md` for a user-visible change.
