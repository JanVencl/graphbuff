# Security policy

## Reporting a vulnerability

Report it privately through GitHub:
[open a security advisory](https://github.com/JanVencl/graphbuff/security/advisories/new)
on this repository. Please do not open a public issue, pull request or discussion for a
vulnerability.

Include what you can of:

```
Affected version or commit:
What an attacker can do:
Steps to reproduce:
Configuration (storage mode, token set or not, TLS on or off):
```

You will get an answer in the advisory. Once a fix is released, the advisory is
published with credit to the reporter, unless you prefer otherwise. There is no bug
bounty.

## Supported versions

Fixes land on `main` and ship in the next release. Only the latest release is supported;
older versions do not get patch releases.

| Version | Supported |
|---------|-----------|
| 0.4.x   | yes       |
| < 0.4   | no        |

## Scope

In scope: `graphbuff-server` (HTTP and Bolt, authentication, roles, TLS, backup and
restore endpoints), `graphbuff-cli`, the embedded engine (`graphbuff` crate) and the
on-disk formats it reads, including a crafted data directory or backup that crashes or
misleads it.

Out of scope: denial of service by an authenticated user running expensive queries
(limit them with `queries.query_timeout_ms` and `queries.max_memory_mb`), servers bound
to a public address without TLS, and the website.

What the server does to protect data, and what it does not do yet, is described in
[docs/Users.md](docs/Users.md) and [docs/Features.md](docs/Features.md).
