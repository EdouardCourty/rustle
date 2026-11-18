# How we would like reports written

## Layout and length
- Plain text, under one page, with these sections in this order: Summary, Affected versions and
  configuration, Reproduction, Impact, Suggested fix.
- One root cause per report. Start the title with the component name (for example `proxy/pipeline:`,
  `policy:`, `relay/chunked:`).

## What every report should state
- The listener involved (plain, TLS HTTP/1.1, h2, SOCKS5 TCP or UDP) and the config keys that matter. Rustle's
  defaults block private ranges: say if the reproducer turns `filter.block_private` off, and why that is fair.
- The attacker's starting position: unauthenticated peer, client with a valid key, or malicious origin.
- A reproducer a maintainer can run: the config, the exact bytes sent (nc, curl or a short script), and the
  observed result. A new scenario for `e2e/src/suites/` or a test in `rustle/src/server/tests/` is best.
- For memory or concurrency bugs, the faulty function and the sequence of events that reaches it.

## Style
- No Markdown tables. Short sentences. No speculation: separate what was observed from what is suspected.
- Say when a finding is only reachable through documented open-relay mode, operator-level access, or a
  non-default setting.
