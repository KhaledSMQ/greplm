<div align="center">

<img src="docs/logo.svg" alt="greplm" width="320" />

**Fast, offline code search _and code intelligence_ for LLM agents.**

Search code, walk the call graph, resolve typed go-to-definition, run structural (AST) search,
query git history, and assemble task-scoped context packs in **milliseconds** —
fully offline, with token-compact output built for the agent loop.

[![Release](https://github.com/KhaledSMQ/greplm/actions/workflows/release.yml/badge.svg)](https://github.com/KhaledSMQ/greplm/actions/workflows/release.yml)
[![crates.io](https://img.shields.io/crates/v/greplm-cli.svg)](https://crates.io/crates/greplm-cli)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](#license)
[![MSRV](https://img.shields.io/badge/rustc-1.90%2B-orange.svg)](https://www.rust-lang.org)
![Platforms](https://img.shields.io/badge/platform-linux%20%7C%20macOS%20%7C%20windows-lightgrey.svg)

[Install](#install) · [Quick start](#quick-start) · [Code intelligence](#code-intelligence) · [Commands](#commands) · [MCP server](#mcp-server) · [Why greplm?](#why-greplm)

</div>

greplm builds a local index of your codebase so agents (and you) can search code, look up
definitions, find references, and read snippets in milliseconds — all without leaving your machine.
On top of trigram search and tree-sitter symbols, greplm layers real **code intelligence**: a
reference/call-edge graph (callers, callees, blast radius), typed go-to-definition, structural
(AST) search, git time-travel, and task-driven **context packs**. The index lives in a `.greplm/`
directory and works fully offline. It ships as both a **CLI** (`greplm`) and an **MCP server**
(`greplm-mcp`) that plugs into tools like Cursor and Claude Desktop.

> **Why it matters for agents:** every query returns compact locations instead of whole files, so
> coding agents stop burning context on "grep, then read the entire file." In practice that's
> **~90%+ fewer tokens** spent reading code — see [Token efficiency](#token-efficiency).

## Demo

<div align="center">

![greplm in action: search, symbols, call graph, go-to-definition, and context packs](docs/demo.gif)

</div>

## Why greplm?

greplm is built specifically for the agent loop — small, structured results over a hot local
index — rather than for humans scrolling a terminal.

| | greplm | `ripgrep` | `ctags` / LSP |
|---|---|---|---|
| Content search (literal/regex/word) | ✅ | ✅ | ❌ |
| Symbol definitions (13 languages) | ✅ | ❌ | ✅ |
| Find references | ✅ | ⚠️ text-only | ✅ |
| Call graph: callers / callees | ✅ | ❌ | ⚠️ LSP only |
| Blast radius (transitive impact) | ✅ | ❌ | ❌ |
| Typed go-to-definition | ✅ | ❌ | ✅ LSP |
| Structural / AST search | ✅ | ❌ | ❌ |
| Git time-travel (blame/history/changed) | ✅ | ❌ | ❌ |
| Task context packs (budgeted) | ✅ | ❌ | ❌ |
| Persistent incremental index | ✅ | ❌ (scans each run) | ⚠️ regenerate |
| Warm daemon (sub-ms queries) | ✅ | ❌ | ❌ |
| Token-compact output for agents | ✅ | ❌ | ❌ |
| Optional offline semantic search | ✅ | ❌ | ❌ |
| MCP server + ready-made agent files | ✅ | ❌ | ❌ |
| Fully offline / no network | ✅ | ✅ | ✅ |

If you just want fast interactive grep, use ripgrep. If you want a queryable index an agent can
hammer thousands of times without re-scanning the tree, use greplm.

## Features

- **Fast content search** — literal, regex, whole-word, with language and path filters.
- **Symbol lookup** — find definitions by exact, prefix, substring, or fuzzy match across Rust,
  Python, JavaScript, TypeScript/TSX, Go, Java, C, C++, C#, Ruby, PHP, Swift, and Dart (Flutter).
- **References** — locate every occurrence of an identifier, definitions first.
- **Call graph** — `callers` / `callees` / `impact` (blast radius) built from a structural
  reference index, plus `xref` for resolved references (definitions, calls, imports).
- **Typed go-to-definition** — `def <file> <line> <col>` resolves the identifier under the cursor
  using scope, usage context, and imports, flagging the unambiguous target.
- **Structural (AST) search** — match a tree-sitter query or a `$NAME` meta-variable pattern,
  trigram-prefiltered so it stays fast.
- **Context packs** — `pack "<task>" --budget N` assembles exactly the code relevant to a task,
  ranked by lexical relevance and call-graph centrality, packed to a token budget.
- **Git time-travel** — `blame`, symbol `history`, and `changed <rev>` annotated with symbols.
- **File outlines & snippets** — read a file's structure or an exact slice with context.
- **Incremental indexing** — only re-indexes what changed; an optional watcher and warm daemon
  keep queries sub-millisecond.
- **Offline & private** — no network calls, nothing leaves your machine.

## Install

**One line** (Linux / macOS / Git Bash on Windows):

```bash
curl -fsSL https://raw.githubusercontent.com/KhaledSMQ/greplm/main/install.sh | sh
```

**Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/KhaledSMQ/greplm/main/install.ps1 | iex
```

**With Rust installed:**

```bash
cargo install --locked --git https://github.com/KhaledSMQ/greplm greplm-cli greplm-mcp
```

This installs `greplm` and `greplm-mcp` into `~/.cargo/bin` (or `~/.local/bin`). Prebuilt binaries
are also published on [GitHub Releases](https://github.com/KhaledSMQ/greplm/releases).

### Add the agent file

greplm ships ready-made agent definitions that teach your coding tool to reach for `greplm`
instead of raw grep. The definitions are baked into the binary, so installing one is a single
offline command:

```bash
greplm agent add cursor          # install into .cursor/agents/ for this project
greplm agent add claude --global # install into ~/.claude/agents/ for every project
greplm agent add                 # auto-detect from existing tool directories
greplm agent list                # show every supported tool and its destination
```

Supported tools and where the file lands (project scope; pass `--global` for the home directory):

| Tool           | `agent add` key | Destination                              |
|----------------|-----------------|------------------------------------------|
| Claude Code    | `claude`        | `.claude/agents/greplm-search.md`        |
| Cursor         | `cursor`        | `.cursor/agents/greplm-search.md`        |
| Gemini CLI     | `gemini`        | `.gemini/agents/greplm-search.md`        |
| GitHub Copilot | `copilot`       | `.github/agents/greplm-search.agent.md`  |
| opencode       | `opencode`      | `.opencode/agent/greplm-search.md`       |
| Kiro           | `kiro`          | `.kiro/agents/greplm-search.md`          |
| Pi             | `pi`            | `.pi/agents/greplm-search.md`            |
| Reasonix       | `reasonix`      | `.reasonix/agents/greplm-search.md`      |

Use `--force` to overwrite an existing file. The raw definitions also live in
[`crates/greplm-cli/agents/`](crates/greplm-cli/agents) if you prefer to copy them manually. Restart the tool (or start a new
session) so it picks up the new agent.

## Quick start

```bash
# Build the index for the current project
greplm index

# Search file contents
greplm search "SegmentWriter"

# Look up a definition
greplm symbols Searcher

# Find references to an identifier
greplm refs SegmentWriter
```

Each query prints compact, ready-to-jump locations:

```console
$ greplm search "SegmentWriter" --limit 4
crates/greplm-core/src/segment.rs:68:12: pub struct SegmentWriter {

$ greplm symbols Searcher --limit 4
function   searcher                 crates/greplm-core/src/lib.rs:129-131
struct     Searcher                 crates/greplm-core/src/search.rs:219-223
function   swap_searcher            crates/greplm-core/src/daemon.rs:50-53
function   read_searcher            crates/greplm-core/src/daemon.rs:46-48

$ greplm refs SegmentWriter --limit 4
crates/greplm-core/src/segment.rs:68:12: pub struct SegmentWriter {
```

That's it — re-run `greplm index` after changes (it's incremental), or keep it automatic with
`greplm watch`.

## Code intelligence

Beyond search, greplm answers the questions an agent actually asks before editing: *who calls
this, what does it call, what breaks if I change it, where is this defined, and give me exactly
the code for this task.*

```console
$ greplm callers references --limit 3
cmd_refs -> references  crates/greplm-cli/src/main.rs:663:17
dispatch -> references  crates/greplm-core/src/daemon.rs:222:32
find_references -> references  crates/greplm-mcp/src/main.rs:323:24

$ greplm impact add_doc --depth 2
d0  function   add_doc            crates/greplm-core/src/segment.rs:114-147
d1  function   index_full         crates/greplm-core/src/indexer.rs:126-198
d1  function   index_incremental  crates/greplm-core/src/indexer.rs:201-374
d2  function   index              crates/greplm-core/src/lib.rs:111-119

$ greplm def crates/greplm-cli/src/main.rs 663 57
* function   references               crates/greplm-core/src/search.rs:488-496

$ greplm ast 'fn $NAME() {}' --lang rust --limit 1
crates/greplm-cli/src/agent.rs:86-88:     fn dest(&self, scope_root: &Path) -> PathBuf { NAME=dest

$ greplm pack "how does incremental indexing work" --budget 4000
# context pack for: how does incremental indexing work
# 6 items, ~3.9k/4000 tokens
## function index_incremental (match)  crates/greplm-core/src/indexer.rs:201-374  [22.4]
  ...
```

The `*` in `def` marks an unambiguous resolution; otherwise candidates are ranked and the agent
sees the alternatives. `impact`, `callers`, and `callees` resolve by name, so treat them as a
fast, high-recall guide rather than a proof. `ast` accepts either a full tree-sitter query
S-expression (with `@captures` and `#eq?`/`#match?` predicates) or the friendly `$NAME` form.

## Commands

Every command accepts the **global options** `-C, --root <dir>` (target another project),
`--no-daemon` (bypass a running daemon), and — for query commands — `--json` (machine-readable
output). They're omitted from the tables below for brevity.

**Indexing**

| Command          | Arguments & key options              | What it does                                       |
|------------------|--------------------------------------|----------------------------------------------------|
| `greplm init`    | —                                    | Create `.greplm/` with a default config (no indexing) |
| `greplm index`   | `[--force]`                          | Build or refresh the index (`--force` rebuilds from scratch) |
| `greplm watch`   | `[--debounce-ms <ms>]`               | Watch the project and re-index on changes (default `300`) |
| `greplm clean`   | —                                    | Delete the `.greplm/` index directory              |

**Querying**

| Command            | Arguments & key options                                                                 | What it does                                  |
|--------------------|-----------------------------------------------------------------------------------------|-----------------------------------------------|
| `greplm search`    | `<query> [-e/--regex] [-i/--ignore-case] [-w/--word] [--lang <id>] [--path <substr>] [--limit <n>] [--offset <n>] [--max-per-file <n>]` | Search file contents (literal / regex / whole-word / filters) |
| `greplm symbols`   | `<name> [--kind <k>] [--exact] [--limit <n>] [--offset <n>]`                             | Look up symbol definitions by name            |
| `greplm refs`      | `<name> [--limit <n>] [--offset <n>]`                                                    | Find references to an identifier (text)       |
| `greplm outline`   | `<file>`                                                                                 | Print the symbol outline of a single file     |
| `greplm snippet`   | `<file> <start> [end] [--context <n>]`                                                   | Print a file slice with surrounding context (default `3`) |
| `greplm summary`   | —                                                                                        | Summarize the indexed repository              |
| `greplm status`    | —                                                                                        | Show index status                             |
| `greplm savings`   | `[-v/--verbose]`                                                                         | Show estimated tokens saved vs. grep+read     |

**Code intelligence**

| Command           | Arguments & key options                                  | What it does                                            |
|-------------------|----------------------------------------------------------|---------------------------------------------------------|
| `greplm xref`     | `<name> [--limit <n>] [--offset <n>]`                    | Resolved references: definitions + call sites + imports |
| `greplm callers`  | `<name> [--limit <n>] [--offset <n>]`                    | Who calls a function/method                             |
| `greplm callees`  | `<name> [--limit <n>] [--offset <n>]`                    | What a function/method calls                            |
| `greplm impact`   | `<name> [--depth <n>] [--limit <n>]`                     | Blast radius via the reverse call graph (default depth `3`) |
| `greplm def`      | `<file> <line> <col>`                                    | Typed go-to-definition for the identifier at a position |
| `greplm refs-at`  | `<file> <line> <col>`                                    | Resolved references for the identifier at a position    |
| `greplm ast`      | `<pattern> --lang <id> [--limit <n>] [--offset <n>]`     | Structural search (tree-sitter query or `$NAME` pattern) |
| `greplm pack`     | `<task> [--budget <tokens>]`                             | Build a token-budgeted context pack for a task (default `8000`) |

**Git time-travel** (requires a git repo)

| Command          | Arguments & key options       | What it does                                              |
|------------------|-------------------------------|-----------------------------------------------------------|
| `greplm blame`   | `<file> <line>`               | Commit, author, and summary that last changed a line      |
| `greplm history` | `<name> [--limit <n>]`        | Commits that touched a symbol's line range (newest first) |
| `greplm changed` | `<rev>`                       | Files changed since a revision, annotated with their symbols |

**Daemon & semantic search**

| Command                  | Arguments & key options                  | What it does                                  |
|--------------------------|------------------------------------------|-----------------------------------------------|
| `greplm serve`           | —                                        | Run the warm-index daemon (serves queries over a socket) |
| `greplm semantic-index`  | `[--model <dir>]`                        | Build the optional semantic (vector) index    |
| `greplm semantic-search` | `<query> [--limit <n>] [--model <dir>]`  | Search the semantic index by meaning           |

**Agent setup**

| Command             | Arguments & key options              | What it does                                          |
|---------------------|--------------------------------------|-------------------------------------------------------|
| `greplm agent add`  | `[tool] [--global] [--force]`        | Install the bundled agent file (auto-detects the tool when omitted) |
| `greplm agent list` | `[--global]`                         | List supported tools and their destination paths      |

Run `greplm <command> --help` for the full flag list. Most query commands support `--limit` /
`--offset` for pagination.

## Usage

```bash
# Set up
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
```

Most query commands accept `--json` for agent consumption and `-C/--root <dir>` to point at a
different project. Set `GREPLM_LOG=debug` for verbose logging.

### Warm daemon (fastest for agent loops)

Run a daemon to keep the index hot in memory with the watcher running; queries then drop to
sub-millisecond:

```bash
greplm serve
```

While it's running, query commands automatically route to it (so does the MCP server). Pass
`--no-daemon` to force an in-process query.

The daemon is what makes greplm fast for agents: a warm socket query is ~sub-ms, versus ~25ms
to cold-open the index per call. Keep it running so that advantage is never lost.

#### Keep it always-on

Run the daemon as a background service that starts at login and restarts if it dies.

**macOS (launchd):**

```bash
contrib/launchd/install-launchd.sh /abs/path/to/project   # defaults to the current dir
```

**Linux (systemd user service):** see [`contrib/systemd/greplm-daemon@.service`](contrib/systemd/greplm-daemon@.service) for the one-time install (it documents the `systemctl --user enable --now` command).

Both serve one project root per instance, log to `<root>/.greplm/daemon.log`, and print their
uninstall command.

### Semantic search (optional)

An optional, fully offline meaning-based search layer behind the `semantic` feature:

```bash
cargo build --release -p greplm-cli --features semantic
greplm semantic-index
greplm semantic-search "parse a regex into a trigram query" --limit 10
```

## Token efficiency

greplm exists to keep coding agents off the "grep, then read whole files" treadmill
that burns context. Every query returns compact locations (and, for `snippet`, an exact
slice) instead of file bodies, so the agent pulls in a few lines rather than thousands.

greplm tracks this automatically. Each query records the grep+read baseline (the full
size of the unique files it referenced) against the size of the payload it actually
returned; `greplm savings` aggregates the estimate (≈4 chars/token, a conservative basis):

```bash
greplm savings            # rolling 24h / 7d / all-time summary
greplm savings --verbose  # also break down by query kind
greplm savings --json     # machine-readable
```

```
  greplm Token Savings
  ================================================================
  Period          Calls   Savings
  ----------------------------------------------------------------
  Last 24h            4   [███████████████░]  ~95.6k tokens (96%)
  Last 7 days         4   [███████████████░]  ~95.6k tokens (96%)
  All time            4   [███████████████░]  ~95.6k tokens (96%)
```

Stats live in `.greplm/savings.jsonl`; set `GREPLM_NO_SAVINGS=1` to disable recording.

To reproduce the efficiency numbers, run the benchmark in [`bench/`](bench/). It runs
against this repository itself and needs only a release build plus `ripgrep` — no
external corpus, embedding model, or third-party tool:

```bash
cargo build --release

# Search efficiency vs the ripgrep + read-whole-files baseline:
python3 bench/run_bench.py

# Context-pack efficiency (budgeted packs vs reading whole files):
python3 bench/context/pack_bench.py
```

A typical run on this repo shows greplm returning the same files as ripgrep with
**~99% fewer tokens** for content search and **~89% fewer** for context packs. See
[`bench/README.md`](bench/README.md) for the methodology.

## MCP server

`greplm-mcp` speaks the Model Context Protocol over stdio and exposes these tools to your agent:

| Tool                  | Purpose                                                       |
|-----------------------|---------------------------------------------------------------|
| `index_project`       | build/refresh the index (incremental or `force`)              |
| `search_code`         | content search (literal / regex / whole-word / filters)       |
| `find_symbol`         | symbol lookup (exact / prefix / substring / fuzzy)            |
| `find_references`     | occurrences of an identifier (definitions first)              |
| `resolved_references` | resolved refs from the structural index (defs + calls + imports) |
| `find_callers`        | who calls a function/method                                   |
| `find_callees`        | what a function/method calls                                  |
| `impact_of`           | blast radius via the reverse call graph                       |
| `goto_definition`     | typed go-to-definition for an identifier at file:line:col     |
| `references_at`       | resolved references for an identifier at file:line:col        |
| `structural_search`   | tree-sitter query / `$NAME` pattern (AST) search              |
| `build_context`       | task-driven, token-budgeted context pack (call this first)    |
| `git_blame`           | commit/author that last changed a line                        |
| `symbol_history`      | commits that touched a symbol                                 |
| `changed_since`       | files (with symbols) changed since a revision                 |
| `get_file_outline`    | symbol outline of one file                                    |
| `read_snippet`        | read a file slice with surrounding context                    |
| `repo_summary`        | language breakdown, file/symbol counts, top directories       |
| `index_status`        | index stats                                                   |

### Client configuration

Cursor / Claude Desktop style `mcp.json`:

```json
{
  "mcpServers": {
    "greplm": {
      "command": "/absolute/path/to/greplm-mcp",
      "args": ["/absolute/path/to/your/project"]
    }
  }
}
```

The first argument sets the project root (defaults to the working directory). All diagnostics go to
stderr; stdout carries only the protocol stream.

## Configuration

`.greplm/config.toml` (created on first index) controls the walk and indexing:

```toml
include = []                       # glob whitelist (empty = all text files)
exclude = ["**/.git/**", "**/node_modules/**", "**/target/**", "**/.greplm/**"]
max_file_size = 4194304            # skip files larger than this (bytes)
respect_gitignore = true
index_hidden = false
backend = "auto"                   # auto | rayon | io-uring
merge_threshold = 16               # auto-compact once segments exceed this
```

## Requirements

- **Platforms:** Linux (x86_64, aarch64), macOS (Intel & Apple Silicon), and Windows (x86_64).
  Prebuilt binaries are published for all of these on [GitHub Releases](https://github.com/KhaledSMQ/greplm/releases).
- **Building from source / `cargo install`:** Rust **1.90+** (MSRV).
- **Runtime:** none. No services, no network — the index lives entirely in `.greplm/`.
- **Semantic search** is opt-in behind the `semantic` feature and builds a local vector index; it
  is otherwise not required.

## Build from source

```bash
cargo build --release
# binaries: target/release/greplm  and  target/release/greplm-mcp
```

## License

Released under the [MIT License](LICENSE). Copyright © 2026 [Khaled Sameer](https://github.com/KhaledSMQ).

---

<div align="center">

**greplm** — code search _and_ code intelligence for the agent loop.

Built with 🦀 Rust · Fully offline · Token-compact by design

[Install](#install) · [Quick start](#quick-start) · [Commands](#commands) · [MCP server](#mcp-server) · [Report a bug](https://github.com/KhaledSMQ/greplm/issues) · [Releases](https://github.com/KhaledSMQ/greplm/releases)

If greplm saves your agents some tokens, consider leaving a ⭐ on [GitHub](https://github.com/KhaledSMQ/greplm).

<sub><a href="#readme">↑ Back to top</a></sub>

</div>
