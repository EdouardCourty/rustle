# Security policy

## Reporting a vulnerability

Please do not open a public issue for a vulnerability. Report it privately:

- [GitHub private vulnerability reporting](https://github.com/EdouardCourty/rustle/security/advisories/new)
  (preferred), or
- email edouard.courty2@gmail.com.

Include the Rustle version (`rustle --version`), the relevant part of the configuration, the listener
involved (plain, TLS, HTTP/2, SOCKS5), and the steps or bytes that reproduce the problem. Reports produced
with the help of automated or AI tools are welcome: say so, and include a reproducer.

I aim to acknowledge a report within 7 days and to tell you my assessment shortly after. This is a
best-effort goal from a single maintainer, not a guarantee.

## Supported versions

Only the latest release receives security fixes.

## What is in scope

Rustle is a network-facing proxy, so the following matter most: bypasses of the destination policy
(`[filter]`: private ranges, denied ranges, allowed ports, DNS rebinding), authentication and ban-list
bypasses, HTTP request or response smuggling, memory-safety bugs, and remote denial of service within the
configured limits. The threat model used for scanning, with severity guidance and what is out of scope, is in
[`.oss-scanner/threat_model.md`](.oss-scanner/threat_model.md).

Running with `auth.api_keys = []` is an open relay by design, and the API key travels in clear text on the
plain and SOCKS5 ports (use the TLS port over the Internet): neither is a vulnerability.

## Disclosure

Reports are handled privately. Once a fix is released, the advisory is published on GitHub.
