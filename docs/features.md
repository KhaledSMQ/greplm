# Features & comparison

## Why greplm?

greplm is built specifically for the agent loop — small, structured results over a hot local
index — rather than for humans scrolling a terminal.

| | greplm | `ripgrep` | `ctags` / LSP |
|---|---|---|---|
| Content search (literal/regex/word) | ✅ | ✅ | ❌ |
| Symbol definitions (14 languages) | ✅ | ❌ | ✅ |
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
