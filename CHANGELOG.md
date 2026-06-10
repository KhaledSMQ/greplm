# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are cut by pushing a `vX.Y.Z` tag, which builds cross-platform binaries,
creates the GitHub release, and publishes the crates to crates.io (see
[CONTRIBUTING.md](CONTRIBUTING.md#releasing)).

## [Unreleased]

## [0.4.0] - 2026-06-11

A storage-engine release: the on-disk format moves to columnar, checksummed,
mmap-backed segments (schema v6) and the whole write path is rebuilt around
sort-based inversion and parallel serialization. Existing indexes rebuild
automatically on the next index operation. On the Linux kernel (93k files,
3.3M symbols) a full index build drops from 66s to 26s and the index shrinks
from 1.0G to 760M, while warm queries answer in ~19 ms.

### Added

- **Integrity checksums (xxh3) on every segment section**, verified when a
  segment is opened: corruption is detected up front and surfaces as a clean,
  recoverable error instead of wrong results or a panic deep in a query.
- **Atomic delete protocol**: deletions are journaled as pending tombstones and
  published in the same atomic manifest write as the new segment, so readers
  always see a consistent index — even if the indexer crashes mid-publish.
  Readers apply the journal lazily; compaction retires it.
- **Tiered auto-compaction**: segments merge automatically once
  `merge_threshold` (default 16) accumulate, smallest first, so incremental
  indexing can't degrade query latency by fragmenting the index.
- **Rename fast-path**: a renamed file with identical content reuses its parsed
  symbols and references instead of being re-parsed from scratch.

### Changed

- **Columnar, mmap-backed symbol/reference tables** (index schema v6; old
  indexes rebuild automatically): side tables are stored as packed columns and
  decoded per-row on demand straight from the memory map. Opening a segment no
  longer parses anything — cold start on a million-symbol index is effectively
  instant and memory stays flat regardless of index size.
- **The index write path was rebuilt**: trigram extraction dedups through a
  thread-local bitset, postings are built by sort-based inversion
  (`par_sort` over packed `(trigram, doc)` keys) with roaring bitmaps
  serialized in parallel chunks, and symbol/reference tables stream through
  incremental columnar builders straight to disk — no intermediate
  materialization of the whole table in memory.
- **Segment merges stream**: compaction k-way-merges postings and side tables
  segment-by-segment instead of materializing the merged index in memory, so
  merging huge segments no longer spikes RSS.
- **Query planning is cardinality-aware** (index schema v4; old indexes rebuild
  automatically): each posting list's cardinality is packed into the trigram FST
  value, so AND-groups intersect only their ~4 rarest trigrams, rarest first,
  without touching the postings blob to plan.
- **Case-insensitive search keeps its trigram filter on `s`/`k` needles**: windows
  containing `s`/`k` previously degraded to full scans (their Unicode fold class
  includes `ſ`/`K`); the planner now enumerates the multi-byte fold variants, so
  common needles like `class`, `list`, and `make` stay index-accelerated with no
  false negatives.
- **Regex queries are filtered by required suffix literals too**, not just prefix
  literals — patterns like `fn \w+_handler` now prune candidates instead of
  scanning every file.
- **Ranked search verifies best-first and stops early**: candidates are verified
  in descending max-possible-score order and verification halts once the
  requested page provably cannot change, instead of reading every candidate file.
- **Daemon hot-swaps are incremental and wait-free**: the shared searcher is now
  an `ArcSwap` (queries never block on a reload), and reloads reuse unchanged
  segments and the warm content cache instead of re-parsing the whole index.
- **Symbol/path lookups are indexed**: per-segment name → symbol/reference maps
  and a path → document map replace full-table scans in definitions, resolved
  references, callers, blast radius, outline, and changed-since.
- **Single-pass response serialization**: the daemon serializes each result once
  (`RawValue`), clients parse it straight into typed results, and the MCP server
  forwards daemon payloads verbatim.
- **`search_code`/`find_references` MCP payloads are grouped by file**
  (`{path, lang, hits: [[line, col, text], …]}`), stating each path/language once
  instead of once per hit to cut tokens on multi-hit files.
- Trigram extraction uses a sort/dedup vector pipeline instead of a `BTreeSet`,
  and indexing no longer clones every file's symbol/reference tables into the
  segment writer.

### Fixed

- **Schema upgrades no longer leak the old index on disk**: a full rebuild
  triggered by an unreadable or version-bumped manifest now sweeps every
  segment file the new manifest doesn't reference. Previously each schema
  bump orphaned the entire previous index in `.greplm/segments/`.
- `impact_of` (blast radius) now stops expanding the call graph as soon as the
  node limit is reached instead of finishing the current depth level.
- The context-pack benchmark harness (`pack_bench.py`) measured a stale JSON
  field after the pack output schema changed; real-world benchmark numbers in
  the README and `bench/projects/RESULTS.md` were re-measured from scratch on
  pristine indexes.

## [0.3.0] - 2026-06-09

### Added

- **`greplm welcome`**: first-run checklist (MCP config, agent files, search) you can
  re-run anytime.
- **`greplm mcp config`**: emits ready-to-paste MCP client JSON on stdout with editor
  paste hints on stderr (`--pretty`, `-q` for scripts).
- **Smarter `greplm agent add`**: auto-detects your editor from project markers
  (`.cursor/`, `CLAUDE.md`, etc.), installs a tool-specific subagent *and* main-loop
  guidance (`AGENTS.md` / `CLAUDE.md` / …), and falls back to a universal `AGENTS.md`
  when nothing is detected.
- **Onboarding banners** after `greplm setup`: a styled summary of what was built plus
  a three-step next-actions guide (MCP, agent files, search).
- **Bundled `greplm-search` subagent** definition for Cursor and other tools that read
  `.cursor/agents/`.

### Changed

- Getting-started, commands, and MCP documentation rewritten around the new setup flow
  (`greplm setup` → `greplm mcp config` → `greplm agent add`).
- Install scripts (`install.sh`, `install.ps1`) now print clearer post-install next
  steps instead of stopping at "done".

### Fixed

- `install.sh` installs binaries via a temp file and atomic rename so macOS code
  signatures are not invalidated mid-write.

## [0.2.0] - 2026-06-09

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
- On-disk segment side tables (`docs`/`syms`/`refs`) now use the compact binary
  `postcard` encoding instead of JSON, cutting both index size and cold-start
  parse time on large trees. Bumps the on-disk schema to v3 (existing indexes are
  transparently rebuilt on the next index operation).

### Fixed

- Reading a corrupt or truncated postings blob no longer panics: a malformed
  posting offset now surfaces as a recoverable `Corrupt` error and falls back to a
  direct scan instead of an out-of-bounds slice. (Found by fuzzing.)
- A full index build (`index_full`) no longer silently swallows a real IO error
  while reading the manifest: genuine read failures now propagate, an
  unparseable/version-mismatched manifest warns and recovers the segment-id
  counter by scanning existing segment files (so a rebuild can't clobber a
  still-live segment), and only a missing manifest falls back to defaults.

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

[Unreleased]: https://github.com/KhaledSMQ/greplm/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/KhaledSMQ/greplm/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/KhaledSMQ/greplm/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/KhaledSMQ/greplm/compare/v0.1.3...v0.2.0
[0.1.3]: https://github.com/KhaledSMQ/greplm/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/KhaledSMQ/greplm/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/KhaledSMQ/greplm/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/KhaledSMQ/greplm/releases/tag/v0.1.0
