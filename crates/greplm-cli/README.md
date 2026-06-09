# greplm-cli

Command-line interface for [greplm](https://github.com/KhaledSMQ/greplm) — fast, offline code search and code intelligence for LLM agents.

Install the `greplm` binary to search code, walk the call graph, resolve go-to-definition, run structural (AST) search, query git history, and assemble task-scoped context packs — all from your terminal, fully offline.

## Install

**One line** (Linux / macOS / Git Bash on Windows):

```bash
curl -fsSL https://raw.githubusercontent.com/KhaledSMQ/greplm/main/install.sh | sh
```

**With Rust:**

```bash
cargo install --locked --git https://github.com/KhaledSMQ/greplm greplm-cli
```

**From this workspace:**

```bash
cargo install --path crates/greplm-cli
```

Prebuilt binaries are on [GitHub Releases](https://github.com/KhaledSMQ/greplm/releases).

### Optional semantic search

```bash
cargo install --path crates/greplm-cli --features semantic
```

## Quick start

```bash
# Build the index for the current project
greplm index

# Search file contents
greplm search "SegmentWriter"

# Look up a definition
greplm symbols Searcher

# Find references
greplm refs SegmentWriter
```

Each query prints compact, ready-to-jump locations:

```console
$ greplm search "SegmentWriter" --path segment.rs --limit 4
crates/greplm-core/src/segment.rs:107:12: pub struct SegmentWriter {
crates/greplm-core/src/segment.rs:114:6: impl SegmentWriter {

$ greplm symbols Searcher --limit 4
function   searcher                 crates/greplm-core/src/lib.rs:204-206
struct     Searcher                 crates/greplm-core/src/search.rs:351-355
```

Re-run `greplm index` after changes (it's incremental), or keep it automatic with `greplm watch`.

## Commands

Every command accepts `-C, --root <dir>` (target another project), `--no-daemon` (bypass a running daemon), and — for query commands — `--json` (machine-readable output).

### Indexing

| Command | Description |
|---------|-------------|
| `greplm init` | Create `.greplm/` with a default config (no indexing) |
| `greplm index [--force]` | Build or refresh the index |
| `greplm watch [--debounce-ms <ms>]` | Re-index on file changes |
| `greplm clean` | Delete the `.greplm/` index directory |
| `greplm serve [--global]` | Run the warm-index daemon |
| `greplm setup` | Build the index and install the always-on global daemon service |
| `greplm doctor [--fix]` | Diagnose (and optionally fix) index, daemon, and version issues |
| `greplm update [--check]` | Self-update to the latest release |

### Querying

| Command | Description |
|---------|-------------|
| `greplm search <query>` | Content search (literal / regex / whole-word) |
| `greplm symbols <name>` | Symbol lookup by name |
| `greplm refs <name>` | Find references to an identifier |
| `greplm outline <file>` | Symbol outline of a file |
| `greplm snippet <file> <start> [end]` | File slice with context |
| `greplm summary` | Repository summary |
| `greplm status` | Index status |
| `greplm savings` | Token savings vs. grep+read baseline |

### Code intelligence

| Command | Description |
|---------|-------------|
| `greplm xref <name>` | Resolved references (defs + calls + imports) |
| `greplm callers <name>` | Who calls a function/method |
| `greplm callees <name>` | What a function/method calls |
| `greplm impact <name>` | Blast radius via reverse call graph |
| `greplm def <file> <line> <col>` | Typed go-to-definition |
| `greplm refs-at <file> <line> <col>` | Resolved references at a position |
| `greplm ast <pattern> --lang <id>` | Structural (AST) search |
| `greplm pack "<task>" [--budget N]` | Token-budgeted context pack |

### Git time-travel

| Command | Description |
|---------|-------------|
| `greplm blame <file> <line>` | Git blame for a line |
| `greplm history <name>` | Commit history of a symbol |
| `greplm changed <rev>` | Files changed since a revision |

### Agent setup

| Command | Description |
|---------|-------------|
| `greplm agent add [tool]` | Install the bundled subagent + main-loop guidance (Cursor, Claude, etc.) |
| `greplm agent list` | List supported tools, subagents, and rules destinations |

Run `greplm <command> --help` for the full flag list.

## Agent files

greplm ships ready-made definitions that teach coding tools to reach for `greplm` instead of raw
grep. Each `agent add` installs two things: a delegated **subagent** (`.../greplm-search.md`) and a
greplm-first block in the tool's always-on **memory file** (`CLAUDE.md`, `AGENTS.md`,
`.cursor/rules/greplm.mdc`, …) that steers the main loop:

```bash
greplm agent add cursor          # .cursor/agents/ + .cursor/rules/greplm.mdc
greplm agent add claude --global # ~/.claude/agents/ + ~/.claude/CLAUDE.md
greplm agent add                 # auto-detect (falls back to AGENTS.md)
greplm agent list
```

The memory block is delimited by `<!-- greplm:begin -->`/`<!-- greplm:end -->` markers, so installs
are idempotent, your content is preserved, and `--force` refreshes only that block. Subagent
definitions live in [`agents/`](agents/) and are embedded in the binary.

## Warm daemon

For agent loops that hammer the index, keep it hot in memory:

```bash
greplm serve
```

Query commands automatically route to the daemon while it runs. Pass `--no-daemon` to force an in-process query.

## Environment

| Variable | Effect |
|----------|--------|
| `GREPLM_LOG=debug` | Verbose logging |
| `GREPLM_NO_SAVINGS=1` | Disable token-savings recording |

## Related crates

- [`greplm-core`](../greplm-core) — indexing and search library
- [`greplm-mcp`](../greplm-mcp) — MCP server for IDE/agent integration

See the [project docs](https://github.com/KhaledSMQ/greplm/blob/main/docs/README.md) for MCP setup, benchmarks, and the full comparison with ripgrep and LSP.

## License

MIT
