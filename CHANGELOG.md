# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are cut by pushing a `vX.Y.Z` tag, which builds cross-platform binaries,
creates the GitHub release, and publishes the crates to crates.io (see
[CONTRIBUTING.md](CONTRIBUTING.md#releasing)).

## [Unreleased]

### Added

- **Property-based equivalence tests**: the trigram-accelerated search is checked
  against a brute-force scan oracle so indexed results provably match grep.
- **Fuzz harnesses** (`cargo-fuzz`, five targets): daemon request deserialization,
  trigram query construction, structural pattern compilation, symbol extraction,
  and segment postings decoding — run continuously and on a weekly schedule in CI.
- **Crash-injection durability tests**: incremental indexing and compaction are
  crashed at every atomic-write point and must recover to a full-rebuild ground truth.
- **Criterion microbenchmarks** for hot paths (trigram extraction, symbol
  extraction, search, candidate filtering), compiled in CI to prevent bit-rot and
  measured on a schedule.
- **End-to-end tests for the CLI and the MCP server**: the CLI is driven as a real
  process; the MCP server is driven through a real Model Context Protocol handshake.
- **Coverage reporting in CI** via `cargo-llvm-cov` (summary + lcov artifact).
- **Feature-matrix CI**: builds, lints, and tests the optional `semantic` and
  `io-uring` features so they can't silently break.
- Real-world benchmark corpora (React, Odoo, Linux kernel).

### Changed

- Internal crate dependency (`greplm-core`) is now declared once in
  `[workspace.dependencies]`, so the published version requirement can no longer
  drift from the actual workspace version.
- Improved JSON output handling across `greplm-cli` and `greplm-core`.

### Fixed

- Reading a corrupt or truncated postings blob no longer panics: a malformed
  posting offset now surfaces as a recoverable `Corrupt` error and falls back to a
  direct scan instead of an out-of-bounds slice. (Found by fuzzing.)

## [0.1.3] - 2026-06-08

### Added

- Grep-parity completeness (`--exhaustive`), index freshness checks, and
  self-healing of a missing or stale index on query.

### Changed

- Warm-daemon query path performance improvements.

## [0.1.2] - 2026-06-08

### Changed

- Documentation improvements, including richer guidance for the bundled agents.

## [0.1.1] - 2026-06-08

### Added

- Additional ready-made code-search agent definitions.

## [0.1.0] - 2026-06-08

- Initial public release: trigram content search, symbol/call-graph intelligence,
  structural (AST) search, git time-travel, token-budgeted context packs, a warm
  query daemon, a CLI, and an MCP server — across 14 languages, fully offline.

[Unreleased]: https://github.com/KhaledSMQ/greplm/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/KhaledSMQ/greplm/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/KhaledSMQ/greplm/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/KhaledSMQ/greplm/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/KhaledSMQ/greplm/releases/tag/v0.1.0
