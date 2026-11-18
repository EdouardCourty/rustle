# How we would like reports written

## Layout and length
- Plain text or simple Markdown, about one page plus the reproducer. Sections in this order: Summary,
  Affected component, Preconditions, Reproduction, Impact, Suggested fix.
- One root cause per report. If the same root cause is reachable from several listeners or paths (plain, TLS,
  h2, SOCKS5, WebSocket), list them all in one report.
- Title: component name first, then what goes wrong (for example `proxy/pipeline: ...`, `policy: ...`,
  `relay/chunked: ...`).

## First lines of every report
- Whether the bug was reproduced by running Rustle, or only reasoned from reading the source.
- The Rustle version (`rustle --version`) or git commit that was tested.

## What every report should state
- The affected file and function, with `file:line`.
- Preconditions: the listener involved and the config keys that matter, and whether they are the defaults,
  commonly used, or unusual (kTLS, `pin_cores`, `filter.block_private = false`, no `api_keys`). Rustle blocks
  private ranges by default: say if the reproducer turns that off and why that is fair.
- The attacker's starting position: unauthenticated peer, client with a valid key, malicious origin, another
  client of the same proxy.
- A reproducer a maintainer can run offline: the config, the exact bytes sent (raw bytes with explicit chunk
  boundaries, or a short script) and the observed result. A new scenario for `e2e/src/suites/` or a test in
  `rustle/src/server/tests/` is best.
- For memory or concurrency bugs: the faulty function and the sequence of events that reaches it, and any
  sanitizer or crash output.
- For denial of service: the measured effect (memory, descriptors, CPU, time) and how many requests or
  connections it took, compared with the configured limit it should have respected.
- Whether it duplicates something in `CHANGELOG.md` or a known gap in the threat model.

## Style
- No Markdown tables. Short sentences, plain facts.
- Wording must match the proof. If a step is not demonstrated, say so instead of assuming it. Do not write
  "RCE", "request smuggling" or "SSRF" unless the reproducer shows it.
- Keep what was observed apart from what is suspected.
