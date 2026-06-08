# Usage

## Common workflows

```bash
# First-run (index + always-on global daemon service)
greplm setup

# Set up manually
greplm init             # scaffold .greplm/config.toml (no indexing yet)

# Indexing
greplm index            # incremental build/refresh
greplm index --force    # full rebuild
greplm watch            # re-index automatically on file changes
greplm clean            # remove .greplm/

# Search file contents
greplm search "tokio" --lang rust
greplm search -e 'fn .*candidates' --path crates/ --limit 20 --json
greplm search "get" --word --limit 20 --offset 20   # whole-identifier + pagination

# Symbols / definitions
greplm symbols Searcher --kind struct --exact
greplm symbols lc       # acronym match: loadConfig / load_config

# References, outlines, snippets
greplm refs SegmentWriter
greplm outline crates/greplm-core/src/trigram.rs
greplm snippet crates/greplm-core/src/trigram.rs 15 25 --context 3

# Repo info
greplm summary
greplm status

# Health check
greplm doctor           # diagnose index freshness, daemon, and version
greplm doctor --fix     # refresh a stale index and install the daemon service
greplm update --check   # see if a newer release is available
```

Most query commands accept `--json` for agent consumption and `-C/--root <dir>` to point at a
different project. `--json` is compact (single-line) by default — add `--pretty` if you want
indented output to read by eye. Set `GREPLM_LOG=debug` for verbose logging. See
[configuration](configuration.md) for `GREPLM_*` environment overrides.

## Warm daemon

Run a daemon to keep the index hot in memory with the watcher running; queries then drop to
sub-millisecond:

```bash
greplm serve
```

While it's running, query commands automatically route to it (so does the MCP server). Pass
`--no-daemon` to force an in-process query.

The daemon is what makes greplm fast for agents: a warm socket query is ~sub-ms, versus ~25ms
to cold-open the index per call. Keep it running so that advantage is never lost.

### One daemon for every project

For running many agents across many repos, use the **global daemon** — a single background
process that serves *every* project on the machine over one per-user socket:

```bash
greplm serve --global
```

It loads each project lazily on first query (its own warm index + watcher) and evicts projects
that go idle, so memory tracks only what you're actively working on. Queries and the MCP server
auto-discover the project root and route to it — no per-project setup. A per-project `greplm serve`
still works and is tried as a fallback.

### Keep it always-on

Run it as a background service that starts at login and restarts if it dies.

**macOS (launchd):**

```bash
contrib/launchd/install-launchd.sh --global              # one daemon for all projects (recommended)
contrib/launchd/install-launchd.sh /abs/path/to/project  # or just one project
```

**Linux (systemd user service):** [`contrib/systemd/greplm-global.service`](../contrib/systemd/greplm-global.service) (all projects, recommended) or the per-project template [`contrib/systemd/greplm-daemon@.service`](../contrib/systemd/greplm-daemon@.service); each file documents its one-time `systemctl --user enable --now` command.

## Semantic search (optional)

An optional, fully offline meaning-based search layer behind the `semantic` feature:

```bash
cargo build --release -p greplm-cli --features semantic
greplm semantic-index
greplm semantic-search "parse a regex into a trigram query" --limit 10
```
