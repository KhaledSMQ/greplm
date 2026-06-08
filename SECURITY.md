# Security Policy

## Supported versions

greplm is pre-1.0 and ships from a single active release line. Security fixes are
made against the latest released `0.1.x` version; please upgrade to the newest
release before reporting.

| Version | Supported |
|---------|-----------|
| latest `0.1.x` | ✅ |
| older releases | ❌ |

## Reporting a vulnerability

Please **do not** open a public GitHub issue for security problems.

Instead, report privately via GitHub's
[private vulnerability reporting](https://github.com/KhaledSMQ/greplm/security/advisories/new)
("Report a vulnerability" under the repository's **Security** tab). If that is
unavailable, contact the maintainer through their
[GitHub profile](https://github.com/KhaledSMQ).

Please include:

- a description of the issue and its impact,
- steps to reproduce (a minimal repository or input is ideal), and
- any relevant version, OS, and configuration details.

You can expect an acknowledgement within a few days. We'll work with you on a fix
and coordinate disclosure once a patched release is available.

## Scope and threat model

greplm runs locally and offline: it indexes a project directory and answers
queries, optionally over a Unix-domain socket (the daemon) or stdio (the MCP
server). It does not phone home and performs no network I/O during indexing or
search.

Inputs worth special attention — and which we fuzz continuously
(see [`fuzz/`](fuzz/)) — include:

- untrusted source files being indexed,
- on-disk index segments (e.g. a corrupted or truncated index), and
- daemon/MCP request payloads.

Reports of crashes, panics, or memory-safety issues reachable from any of these
inputs are in scope and welcome.
