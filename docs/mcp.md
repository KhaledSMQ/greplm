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
