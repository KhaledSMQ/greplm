# MCP server

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

## Client configuration

Generate copy-paste JSON with resolved absolute paths:

```bash
cd your-project
greplm mcp config          # JSON on stdout, paste hints on stderr
greplm mcp config --pretty # indented JSON (same content)
greplm mcp config -q       # JSON only (for scripts)
```

Example output:

```json
{
  "mcpServers": {
    "greplm": {
      "command": "/Users/you/.cargo/bin/greplm-mcp",
      "args": ["/Users/you/projects/my-app"]
    }
  }
}
```

**Where to paste it**

| Client | Config file |
|--------|-------------|
| Cursor (project) | `.cursor/mcp.json` |
| Cursor (global) | Cursor Settings → MCP |
| Claude Desktop | `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) |
| VS Code | `.vscode/mcp.json` |

The first `args` entry sets the project root. All diagnostics go to stderr; stdout carries only
the protocol stream.

Also run `greplm agent add` so your editor knows *when* to reach for greplm (see
[Getting started — Add the agent file](getting-started.md#add-the-agent-file)).

## Output format

Tool results are returned as **compact JSON** (single-line, no pretty-print indentation) — the
consumer is an LLM, so every byte of whitespace would be wasted context.

Code snippets are encoded as a single text blob rather than an array of per-line objects:
`read_snippet` returns `{ path, start_line, end_line, total_lines, text }` and each
`build_context` item returns `{ ..., snippet_start, code }`, where `text`/`code` are the lines
joined by `\n`. Line numbers are implicit — the i-th line is `start_line + i` (or
`snippet_start + i`) — so they are never repeated on the wire. Together these keep responses a
fraction of the size of an equivalent grep-and-read.
