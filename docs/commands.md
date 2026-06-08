# Commands

Every command accepts the **global options** `-C, --root <dir>` (target another project),
`--no-daemon` (bypass a running daemon), and — for query commands — `--json` (machine-readable
output). They're omitted from the tables below for brevity.

Run `greplm <command> --help` for the full flag list. Most query commands support `--limit` /
`--offset` for pagination.

## Indexing

| Command          | Arguments & key options              | What it does                                       |
|------------------|--------------------------------------|----------------------------------------------------|
| `greplm init`    | —                                    | Create `.greplm/` with a default config (no indexing) |
| `greplm index`   | `[--force]`                          | Build or refresh the index (`--force` rebuilds from scratch) |
| `greplm watch`   | `[--debounce-ms <ms>]`               | Watch the project and re-index on changes (default `300`) |
| `greplm clean`   | —                                    | Delete the `.greplm/` index directory              |

## Querying

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

## Code intelligence

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

## Git time-travel

Requires a git repo.

| Command          | Arguments & key options       | What it does                                              |
|------------------|-------------------------------|-----------------------------------------------------------|
| `greplm blame`   | `<file> <line>`               | Commit, author, and summary that last changed a line      |
| `greplm history` | `<name> [--limit <n>]`        | Commits that touched a symbol's line range (newest first) |
| `greplm changed` | `<rev>`                       | Files changed since a revision, annotated with their symbols |

## Daemon & semantic search

| Command                  | Arguments & key options                  | What it does                                  |
|--------------------------|------------------------------------------|-----------------------------------------------|
| `greplm serve`           | —                                        | Run the warm-index daemon (serves queries over a socket) |
| `greplm semantic-index`  | `[--model <dir>]`                        | Build the optional semantic (vector) index    |
| `greplm semantic-search` | `<query> [--limit <n>] [--model <dir>]`  | Search the semantic index by meaning           |

## Agent setup

| Command             | Arguments & key options              | What it does                                          |
|---------------------|--------------------------------------|-------------------------------------------------------|
| `greplm agent add`  | `[tool] [--global] [--force]`        | Install the bundled agent file (auto-detects the tool when omitted) |
| `greplm agent list` | `[--global]`                         | List supported tools and their destination paths      |
