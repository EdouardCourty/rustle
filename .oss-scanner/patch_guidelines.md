# How we would like patches made

## Size and scope of a fix
- As narrow as possible: fix the faulty function or check. No refactoring, no renames, no reformatting of
  untouched code.
- Keep the hot path allocation-free: reuse `buf`, `resp_buf`, `rewrite_buf` and `origin_key`; no per-request
  `setsockopt`; do not invalidate `RequestHead` spans (no `buf.clear()` or reallocation between parse and use).
- Do not widen request retry (idempotent method with a fully buffered body only) and do not change the pool
  acceptance rules (complete, keep-alive, framed, no over-read).
- Stay inside the existing `unsafe` boundaries; any new `unsafe` block needs a `SAFETY` comment.

## Code style
- Rust 2021, `cargo fmt`, `cargo clippy -D warnings` clean. Few comments: only where the code is not obvious.
  English only. No history or milestone labels in comments.
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
