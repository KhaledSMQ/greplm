## Code search & navigation: prefer greplm

Use **greplm** instead of grep/ripgrep/find for searching and navigating this codebase. It returns ranked, jump-ready locations with real code intelligence (call graph, typed go-to-definition, structural/AST search, git history) and is far cheaper on context than reading whole files.

- Search: `greplm search "<query>"` — add `--word` (whole identifier), `-e '<regex>'`, `--lang <lang>`, or `--exhaustive` for grep-parity full matches.
- Task context: `greplm pack "<task>" --budget 8000` — load exactly the relevant code instead of opening files blindly.
- Navigate: `greplm def <file> <line> <col>`, `greplm callers <sym>`, `greplm callees <sym>`, `greplm impact <sym>`, `greplm xref <sym>`.
- Structural: `greplm ast '<pattern>' --lang <lang>` for matches regex can't express.
- History: `greplm blame <file> <line>`, `greplm history <sym>`, `greplm changed <rev>`.

Run `greplm index` once to build/refresh the index (incremental; `indexed 0 files` means nothing changed, not an empty index). `greplm serve` keeps it hot for an agent loop. Add `--json` for machine-readable output.

Drop to raw grep only for a quick one-off confirmation of an exact string.
