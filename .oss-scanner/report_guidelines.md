# How we would like reports written

## Layout and length
- Plain text or simple Markdown, about one page plus the reproducer, with these sections in this order:
  Summary, Affected component, Preconditions, Reproduction, Impact, Suggested fix.
- One root cause per report. If the same root cause is reachable from several listeners or paths, list them
  all in one report. Start the title with the component name (for example `proxy/pipeline:`, `policy:`,
  `relay/chunked:`).

## What every report should state
- The affected file and function, and the Rustle version or git commit that was tested.
- The listener involved (plain, TLS HTTP/1.1, h2, SOCKS5 TCP or UDP) and the config keys that matter. Rustle's
  defaults block private ranges: say if the reproducer turns `filter.block_private` off, and why that is fair.
- The attacker's starting position: unauthenticated peer, client with a valid key, or malicious origin.
- A reproducer a maintainer can run: the config, the exact bytes sent (nc, curl or a short script), and the
  observed result. A new scenario for `e2e/src/suites/` or a test in `rustle/src/server/tests/` is best.
- Whether the setting that reaches the bug is the default, common, or unusual (for example kTLS, `pin_cores`,
  or `block_private = false`).
- For memory or concurrency bugs, the faulty function and the sequence of events that reaches it.

## Style
- No Markdown tables. Short sentences. Wording must match the proof: if a step is not demonstrated, say so
  instead of assuming it. Do not write "RCE", "request smuggling" or "SSRF" unless the reproducer shows it.
- Separate what was observed from what is suspected.
- Say when a finding is only reachable through documented open-relay mode, operator-level access, or a
  non-default setting.
