# greplm-mcp

[Model Context Protocol](https://modelcontextprotocol.io/) (MCP) stdio server for [greplm](https://github.com/KhaledSMQ/greplm).

Exposes the greplm trigram code index to LLM agents in Cursor, Claude Desktop, and other MCP clients. All logging goes to stderr; stdout is reserved for the protocol.

## Install

**With Rust:**

```bash
cargo install --locked --git https://github.com/KhaledSMQ/greplm greplm-mcp
```

**From this workspace:**

```bash
cargo install --path crates/greplm-mcp
```

Install both binaries at once:

```bash
cargo install --locked --git https://github.com/KhaledSMQ/greplm greplm-cli greplm-mcp
```

## Client configuration

Point your MCP client at the `greplm-mcp` binary and pass the project root as the first argument:

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

**Cursor** — add to `.cursor/mcp.json` or global MCP settings.

**Claude Desktop** — add to `claude_desktop_config.json`.

The server only indexes paths under the configured root. A caller-supplied `root` in `index_project` is honored only when it stays within that boundary.

## Tools

Call `index_project` once before searching, or after large changes. For new tasks, prefer `build_context` to load exactly the relevant code on a token budget instead of reading whole files.

| Tool | Purpose |
|------|---------|
| `index_project` | Build or refresh the index (incremental or `force`) |
| `search_code` | Content search (literal / regex / whole-word / filters) |
| `find_symbol` | Symbol lookup (exact / prefix / substring / fuzzy) |
| `find_references` | Occurrences of an identifier (definitions first) |
| `resolved_references` | Resolved refs from the structural index |
| `find_callers` | Who calls a function/method |
| `find_callees` | What a function/method calls |
| `impact_of` | Blast radius via the reverse call graph |
| `goto_definition` | Typed go-to-definition at file:line:col |
| `references_at` | Resolved references at file:line:col |
| `structural_search` | Tree-sitter query / `$NAME` pattern search |
| `build_context` | Task-driven, token-budgeted context pack |
| `git_blame` | Commit/author that last changed a line |
| `symbol_history` | Commits that touched a symbol |
| `changed_since` | Files (with symbols) changed since a revision |
| `get_file_outline` | Symbol outline of one file |
| `read_snippet` | Read a file slice with surrounding context |
| `repo_summary` | Language breakdown, file/symbol counts |
| `index_status` | Index stats |

All tools return JSON payloads suitable for agent consumption.

## Typical agent workflow

1. **`index_project`** — ensure the project is indexed
2. **`build_context`** — load a token-budgeted context pack for the task
3. **`search_code`** / **`find_symbol`** / **`goto_definition`** — drill into specifics
4. **`find_callers`** / **`find_callees`** / **`impact_of`** — understand dependencies before editing
5. **`read_snippet`** — fetch exact code at reported line numbers

## Run manually

```bash
greplm-mcp /path/to/project
```

The process speaks MCP over stdin/stdout. Use an MCP inspector or your IDE to interact with it.

## Environment

| Variable | Effect |
|----------|--------|
| `GREPLM_LOG=debug` | Verbose logging to stderr |
| `GREPLM_NO_SAVINGS=1` | Disable token-savings recording |

## Related crates

- [`greplm-core`](../greplm-core) — indexing and search library
- [`greplm-cli`](../greplm-cli) — command-line interface (`greplm`)

See the [project README](https://github.com/KhaledSMQ/greplm#mcp-server) for the full tool reference and agent setup (`greplm agent add`).

## License

MIT
