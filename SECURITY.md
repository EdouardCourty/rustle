# Security policy

## Reporting a vulnerability

Please do not open a public issue for a vulnerability.

Report it privately through [GitHub private vulnerability reporting](https://github.com/EdouardCourty/rustle/security/advisories/new)
or by email to edouard.courty2@gmail.com. Include the version (`rustle --version`), the relevant part of the
configuration, and the steps or bytes that reproduce the problem.

## Supported versions

Only the latest release receives security fixes.

## Scope

Rustle is a network-facing proxy. Of particular interest: bypasses of the destination policy
(`[filter]`), authentication or ban-list bypasses, HTTP request or response smuggling, memory-safety bugs,
and remote denial of service. Running with `auth.api_keys = []` is an open relay by design and is not a
vulnerability.
