# Real-world benchmarks

The benchmarks in [`bench/`](../) run against the greplm repo itself so anyone can
reproduce them from a clean checkout. These ones run the *same* methodology
against three large, real codebases to show how it holds up at scale:

| Project | Language | Files indexed |
|---------|----------|---------------|
| [React](https://github.com/facebook/react) | JavaScript / TypeScript | ~6.7k |
| [Odoo 18](https://github.com/odoo/odoo) | Python / JS / XML | ~41k |
| [Linux kernel](https://github.com/torvalds/linux) | C | ~93k |

Latest run: **[RESULTS.md](RESULTS.md)**.

## Reproduce

```bash
cargo build --release                      # or set $GREPLM_BIN
# point projects.json at your local checkouts, then:
python3 bench/projects/run_all.py --index  # build each index, then benchmark
# already indexed? drop --index. one project only:
python3 bench/projects/run_all.py --only linux
```

Requires [`ripgrep`](https://github.com/BurntSushi/ripgrep) (`rg`) on `PATH` for the
baseline. Edit the `root` fields in [`projects.json`](projects.json) to your paths.

## What it measures

For each project it runs the two existing benchmark scripts and aggregates them:

- **Content search** ([`run_bench.py`](../run_bench.py)) — for each realistic
  identifier, compare `greplm search` against ripgrep-finds-files + read-each-file-in-full.
  ripgrep runs in literal mode (`-F`) and greplm with `--max-per-file 1`, so both
  engines look for the same string and surface the same *files* (recall stays
  meaningful even when one query matches thousands of files).
- **Context packs** ([`context/pack_bench.py`](../context/pack_bench.py)) — for each
  task, compare a budgeted `greplm pack` against reading every file the pack drew from.
- **Warm latency** — median per-query time against the always-on daemon (index built
  once, queried hot), versus ripgrep re-scanning the whole tree on every query.

`tokens ~= characters / 4`, applied identically to both engines.

## Files

- [`projects.json`](projects.json) — project roots, languages, and recorded index stats.
- `*.queries.json` — literal identifiers per project for the search benchmark.
- `*.tasks.json` — natural-language tasks per project for the context-pack benchmark.
- [`run_all.py`](run_all.py) — orchestrator; writes `RESULTS.md` and `results.json`.
