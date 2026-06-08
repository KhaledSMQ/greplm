# greplm-core

Core indexing and search engine for [greplm](https://github.com/KhaledSMQ/greplm): a fast, offline trigram code index with code intelligence built for LLM agents.

The index lives in a `.greplm/` directory at the project root. Immutable, mmap-backed segments combine trigram FSTs, roaring posting lists, and symbol tables. Search filters candidates by trigram intersection, then verifies matches with the real literal/regex matcher.

## Features

- **Trigram content search** — literal, regex, whole-word, with language and path filters
- **Symbol extraction** — tree-sitter parsing for Rust, Python, JavaScript, TypeScript/TSX, Go, Java, C, C++, C#, Ruby, PHP, Swift, and Dart
- **Code intelligence** — references, call graph (callers/callees/blast radius), typed go-to-definition, structural (AST) search
- **Context packs** — task-driven, token-budgeted code assembly ranked by relevance and call-graph centrality
- **Git integration** — blame, symbol history, changed-files-since-revision
- **Incremental indexing** — only re-indexes what changed; optional file watcher and warm daemon
- **Fully offline** — no network calls

## Install

Add as a path or git dependency (the crate is not published separately on crates.io today; use the workspace or git URL):

```toml
[dependencies]
greplm-core = { path = "../greplm-core" }
# or
greplm-core = { git = "https://github.com/KhaledSMQ/greplm" }
```

Optional features:

```toml
greplm-core = { path = "../greplm-core", features = ["semantic"] }
```

| Feature | Description |
|---------|-------------|
| `semantic` | Offline semantic (vector) search with a trained Model2Vec embedder |
| `io-uring` | Linux-only io_uring ingest backend |

## Quick start

```rust
use greplm_core::search::{SearchQuery, SymbolQuery};
use greplm_core::Greplm;

fn main() -> greplm_core::Result<()> {
    let g = Greplm::discover(".")?;
    g.index(false)?;

    let searcher = g.searcher()?;

    let hits = searcher.search(&SearchQuery {
        pattern: "SegmentWriter".into(),
        limit: 10,
        ..Default::default()
    })?;

    let symbols = searcher.symbols(&SymbolQuery {
        name: "Searcher".into(),
        limit: 10,
        ..Default::default()
    })?;

    Ok(())
}
```

## API overview

### `Greplm`

The main entry point for a project and its index.

| Method | Description |
|--------|-------------|
| `Greplm::open(root)` | Open (and lazily initialize) the index for a project root |
| `Greplm::discover(start)` | Walk up from `start` to find the nearest `.greplm/` directory |
| `index(force)` | Build or refresh the index (incremental unless `force`) |
| `searcher()` | Open a `Searcher` over the current index |
| `status()` | Report segment count, document/symbol counts, last index time |
| `watch(debounce, on_change)` | Re-index incrementally on file changes |
| `compact()` | Merge segments and drop tombstoned documents |
| `clean()` | Remove the entire `.greplm/` directory |

### `Searcher`

Query interface over a loaded index. Key methods:

- `search` — content search
- `symbols` — symbol lookup (exact, prefix, substring, fuzzy)
- `references` / `references_resolved` — text and structural references
- `callers` / `callees` / `blast_radius` — call graph navigation
- `definition` / `references_of` — typed go-to-definition at a position
- `structural_search` — tree-sitter query or `$NAME` meta-variable patterns
- `context_pack` — token-budgeted task context
- `outline` / `read_snippet` — file structure and slices
- `blame` / `symbol_history` / `changed_since` — git time-travel
- `summary` — repository statistics

### Other modules

| Module | Purpose |
|--------|---------|
| `config` | `.greplm/config.toml` parsing |
| `lang` | Language detection and tree-sitter wiring |
| `client` / `daemon` / `proto` | Warm-index daemon over a Unix socket |
| `semantic` | Optional vector search (`semantic` feature) |
| `savings` | Token-efficiency tracking |

## Index layout

```
.greplm/
├── config.toml      # project configuration
├── meta.bin         # segment manifest and counts
├── cache/           # per-file hash cache for incremental indexing
├── segments/        # immutable mmap-backed index segments
└── savings.jsonl    # token-savings log (optional)
```

## Related crates

- [`greplm-cli`](../greplm-cli) — command-line interface (`greplm`)
- [`greplm-mcp`](../greplm-mcp) — MCP stdio server (`greplm-mcp`)

See the [project README](https://github.com/KhaledSMQ/greplm#readme) for the full feature list, benchmarks, and agent setup.

## License

MIT
