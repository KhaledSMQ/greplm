<div align="center">

<img src="docs/logo.svg" alt="greplm" width="320" />

**Fast, offline code search _and code intelligence_ for LLM agents.**

Coding agents burn most of their context window grepping blind and reading whole files.
greplm gives them a hot local index that answers in **milliseconds** and returns just the
lines that matter — search, call graph, typed go-to-definition, AST search, git history,
and task-scoped context packs, with token-compact output built for the agent loop.

[![Release](https://github.com/KhaledSMQ/greplm/actions/workflows/release.yml/badge.svg)](https://github.com/KhaledSMQ/greplm/actions/workflows/release.yml)
[![crates.io](https://img.shields.io/crates/v/greplm-cli.svg)](https://crates.io/crates/greplm-cli)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](#license)
[![MSRV](https://img.shields.io/badge/rustc-1.90%2B-orange.svg)](https://www.rust-lang.org)
![Platforms](https://img.shields.io/badge/platform-linux%20%7C%20macOS%20%7C%20windows-lightgrey.svg)

[Install](#install) · [Quick start](#quick-start) · [Use it from your agent](#use-it-from-your-agent) · [Why greplm?](#why-greplm) · [Docs](docs/README.md)

</div>

## Why greplm?

When an agent does `grep`, then reads three whole files to find one function, it spends
thousands of tokens to learn almost nothing. greplm flips that: it builds a persistent,
incremental index once, then answers every query with compact, ready-to-jump locations —
not file bodies.

On this repo, against a `ripgrep` + read-whole-files baseline, greplm returns the **same
files** with:

- **~99% fewer tokens** for content search
- **~89% fewer tokens** for context packs

It tracks this for you — run `greplm savings` to see your own numbers:

```
  greplm Token Savings
  ================================================================
  Period          Calls   Savings
  ----------------------------------------------------------------
  Last 24h            4   [███████████████░]  ~95.6k tokens (96%)
```

And it goes beyond grep: greplm understands your code well enough to walk the call graph,
resolve typed go-to-definition, and assemble exactly the code relevant to a task — across
**14 languages**, fully offline, nothing leaving your machine.

## Demo

<div align="center">

![greplm in action: search, symbols, call graph, go-to-definition, and context packs](docs/demo.gif)

</div>

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/KhaledSMQ/greplm/main/install.sh | sh
```

Or with Rust: `cargo install --locked --git https://github.com/KhaledSMQ/greplm greplm-cli greplm-mcp`

Prebuilt binaries are on [GitHub Releases](https://github.com/KhaledSMQ/greplm/releases). See
[Getting started](docs/getting-started.md) for Windows, agent file setup, and build-from-source.

## Quick start

Build the index once, then query it as many times as you like — no re-scanning the tree:

```bash
greplm index                    # build the index (incremental on re-runs)
greplm search "SegmentWriter"   # search file contents
greplm symbols Searcher         # look up definitions
greplm refs SegmentWriter       # find references
```

Every result is a compact, jump-ready location instead of a wall of file text:

```console
$ greplm search "SegmentWriter" --path segment.rs --limit 4
crates/greplm-core/src/segment.rs:107:12: pub struct SegmentWriter {
crates/greplm-core/src/segment.rs:114:6: impl SegmentWriter {

$ greplm symbols Searcher --limit 4
function   searcher                 crates/greplm-core/src/lib.rs:204-206
struct     Searcher                 crates/greplm-core/src/search.rs:351-355
```

Re-run `greplm index` after changes (it's incremental), or use `greplm watch` to keep it fresh.

## Code intelligence

This is where greplm leaves grep behind. It answers the questions an agent actually asks
*before* editing: who calls this, what breaks if I change it, where is this defined, and
*give me exactly the code for this task*.

```console
$ greplm callers references --limit 3          # who calls this function
cmd_refs -> references  crates/greplm-cli/src/main.rs:896:17
dispatch -> references  crates/greplm-core/src/daemon.rs:263:32

$ greplm impact add_doc --depth 2 --limit 4    # blast radius via reverse call graph
d0  function   add_doc                  crates/greplm-core/src/segment.rs:132-165
d1  function   index_full               crates/greplm-core/src/indexer.rs:187-262
d1  function   index_incremental        crates/greplm-core/src/indexer.rs:265-454

$ greplm def crates/greplm-cli/src/main.rs 896 57   # typed go-to-definition
* function   references               crates/greplm-core/src/search.rs:543-551

$ greplm ast 'fn $NAME() {}' --lang rust --limit 1  # structural (AST) search
crates/greplm-cli/src/agent.rs:86-88:     fn dest(&self, scope_root: &Path) -> PathBuf { NAME=dest

$ greplm pack "how does incremental indexing work" --budget 4000   # task context pack
# context pack for: how does incremental indexing work
# 15 items, ~3489/4000 tokens
## function index_incremental (match)  crates/greplm-core/src/indexer.rs:265-454  [17.9]
  ...
```

There's also `xref`, `callees`, `refs-at`, and git time-travel (`blame`, `history`, `changed`).
See [Code intelligence](docs/code-intelligence.md) for the full tour.

## Use it from your agent

greplm ships as an **MCP server** (`greplm-mcp`) so agents like Cursor and Claude Desktop can
call it directly. Drop this into your `mcp.json`:

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

Then teach your tool to reach for greplm instead of raw grep with a bundled agent file:

```bash
greplm agent add cursor          # install into .cursor/
greplm agent add claude --global # install into ~/.claude/
greplm agent list                # see supported tools
```

See the [MCP guide](docs/mcp.md) for the full list of exposed tools.

## How it compares

If you just want fast interactive grep, use `ripgrep`. If you want a queryable index an agent
can hammer thousands of times without re-scanning the tree, use greplm.

| | greplm | `ripgrep` | `ctags` / LSP |
|---|:---:|:---:|:---:|
| Content search (literal/regex/word) | ✅ | ✅ | ❌ |
| Symbol definitions (14 languages) | ✅ | ❌ | ✅ |
| Call graph: callers / callees / impact | ✅ | ❌ | ⚠️ LSP only |
| Typed go-to-definition | ✅ | ❌ | ✅ LSP |
| Structural / AST search | ✅ | ❌ | ❌ |
| Git time-travel (blame/history/changed) | ✅ | ❌ | ❌ |
| Task context packs (budgeted) | ✅ | ❌ | ❌ |
| Persistent incremental index | ✅ | ❌ scans each run | ⚠️ regenerate |
| Warm daemon (sub-ms queries) | ✅ | ❌ | ❌ |
| Token-compact output for agents | ✅ | ❌ | ❌ |
| MCP server + ready-made agent files | ✅ | ❌ | ❌ |
| Fully offline / no network | ✅ | ✅ | ✅ |

Full breakdown: [Features & comparison](docs/features.md).

## Documentation

| Guide | Description |
|-------|-------------|
| [Getting started](docs/getting-started.md) | Install, index, add agent files |
| [Usage](docs/usage.md) | Workflows, daemon, semantic search |
| [Code intelligence](docs/code-intelligence.md) | Call graph, go-to-definition, context packs |
| [Commands](docs/commands.md) | Full CLI reference |
| [MCP server](docs/mcp.md) | MCP tools and client setup |
| [Token efficiency](docs/token-efficiency.md) | How greplm saves agent context |

Full index: **[docs/README.md](docs/README.md)**

## License

Released under the [MIT License](LICENSE). Copyright © 2026 [Khaled Sameer](https://github.com/KhaledSMQ).

---

<div align="center">

**greplm** — code search _and_ code intelligence for the agent loop.

Built with 🦀 Rust · Fully offline · Token-compact by design

[Documentation](docs/README.md) · [Releases](https://github.com/KhaledSMQ/greplm/releases) · [Report a bug](https://github.com/KhaledSMQ/greplm/issues)

</div>
