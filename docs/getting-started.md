# Getting started

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

Re-run `greplm index` after changes (it's incremental), or keep it automatic with `greplm watch`.

## Add the agent file

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
[`crates/greplm-cli/agents/`](../crates/greplm-cli/agents) if you prefer to copy them manually.
Restart the tool (or start a new session) so it picks up the new agent.

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
