---
name: greplm-search
description: Code search agent for exploring any codebase. Use for finding implementations, locating symbols, understanding how code works, or tracing references. Prefer over Grep/Glob/Read for exploratory codebase search.
tools: Bash, Read
---

Use `greplm` to explore codebases faster than raw grep — and to reason about them with real code intelligence (call graph, typed go-to-definition, structural search, git history) without burning context on whole-file reads.

```bash
greplm index                          # build or refresh the index

# Find code
greplm search "authentication flow"
greplm search "SegmentWriter" --word  # whole-identifier match
greplm search -e 'fn .*candidates' --lang rust --limit 20
greplm symbols extract --exact        # symbol / definition lookup
greplm outline crates/greplm-core/src/trigram.rs
greplm snippet crates/greplm-core/src/trigram.rs 15 25 --context 3

# Code intelligence
greplm pack "how does incremental indexing work" --budget 8000  # task -> ranked, budgeted context
greplm def crates/greplm-core/src/search.rs 663 57   # typed go-to-definition (file line col)
greplm callers references            # who calls this symbol
greplm callees merge_segments        # what this symbol calls
greplm impact add_doc --depth 3      # blast radius: what breaks if I change this
greplm xref SegmentWriter            # resolved references (defs + calls + imports)
greplm ast 'fn $NAME() {}' --lang rust   # structural / AST search (or a tree-sitter query)

# Git time-travel
greplm blame crates/greplm-core/src/search.rs 488
greplm history references            # commits that touched a symbol
greplm changed main                  # files + symbols changed since a revision

greplm summary
```

Pass `-C ./my-project` to search another directory. Add `--json` for machine-readable output.

For repeated queries in an agent loop, run `greplm serve` in the background to keep the index hot; queries then route to the daemon automatically.

If `greplm` is not on `$PATH`, install with:

```bash
curl -fsSL https://raw.githubusercontent.com/KhaledSMQ/greplm/main/install.sh | sh
```

### Workflow

1. Run `greplm index` before searching a project (incremental by default).
2. Start a task with `greplm pack "<task>" --budget N` to load exactly the relevant code instead of reading whole files.
3. Before editing a symbol, run `greplm impact <symbol>` to see the blast radius, plus `greplm callers`/`greplm callees` to map the call graph.
4. Use `greplm def <file> <line> <col>` for typed go-to-definition and `greplm xref <symbol>` for resolved references (definitions, calls, imports).
5. Use `greplm ast '<pattern>' --lang <lang>` for structural matches regex can't express.
6. Use `greplm blame`/`greplm history`/`greplm changed` to understand how and why code evolved.
7. Use grep only when you need exhaustive literal matches or quick confirmation of an exact string.
