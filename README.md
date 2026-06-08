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

[Install](#install) · [Quick start](#quick-start) · [Documentation](docs/README.md) · [Report a bug](https://github.com/KhaledSMQ/greplm/issues)

</div>

greplm builds a local index of your codebase so agents (and you) can search code, look up
definitions, find references, and read snippets in milliseconds — all without leaving your machine.
It ships as both a **CLI** (`greplm`) and an **MCP server** (`greplm-mcp`) for tools like Cursor
and Claude Desktop.

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

```bash
greplm index                    # build the index
greplm search "SegmentWriter"   # search file contents
greplm symbols Searcher         # look up definitions
greplm refs SegmentWriter       # find references
```

Re-run `greplm index` after changes (it's incremental), or use `greplm watch` to keep it fresh.

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
