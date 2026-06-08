# greplm benchmarks

Reproducible benchmarks that run against **this repository itself**. They need only:

- a release build of greplm (`cargo build --release`, or set `$GREPLM_BIN`), and
- [`ripgrep`](https://github.com/BurntSushi/ripgrep) (`rg`) on your `PATH`.

No external corpus, no embedding model, and no third-party search engine are
required, so anyone can reproduce the numbers from a clean checkout.

```bash
cargo build --release
python3 bench/run_bench.py            # content-search token efficiency
python3 bench/context/pack_bench.py   # context-pack token efficiency
```

> Build the index first if you haven't: `greplm index` (the scripts search the
> existing `.greplm/` index, so it should reflect the current tree).

## `run_bench.py` — content-search efficiency

For each query in [`queries.json`](queries.json) — a literal identifier a coding
agent would realistically grep for — it compares two ways to find the code:

| | what the agent does | cost counted |
|---|---|---|
| **baseline** | `rg` to find matching files, then read each file **in full** | total characters of those files |
| **greplm** | `greplm search` and read only the returned hit lines | characters of the compact payload |

Tokens are estimated as `characters / 4`. It reports, per query and in aggregate:

- **saved** — `1 − greplm_tokens / baseline_tokens`
- **recall** — fraction of ripgrep's files that greplm also surfaced (a
  correctness check: greplm should not miss matches)
- **latency** — wall-clock per engine (greplm runs cold, with `--no-daemon`; the
  warm daemon is faster still)

The `bench/` directory is excluded from both engines so a query like
`SegmentWriter` is judged on the source, not on the `queries.json` that defines it.

Options: `--root <repo>` (default: this repo), `--k <n>` (max greplm hits per
query), `--queries <file>`, `--out <results.json>`.

### Example run

```
query              rg files gl files  recall  baseline   greplm   saved   rg ms   gl ms
---------------------------------------------------------------------------------------
segment-writer           12       12   100%     22.7k      474   97.9%   16.0   11.6
trigram                  19       19   100%     67.0k     1.2k   98.1%    7.2    8.4
...
=== SUMMARY (tokens ~= chars/4; baseline = ripgrep + read whole files) ===
baseline : 440.6k tokens
greplm   : 5.6k tokens  ->  98.7% fewer (435.0k saved)
recall   : 100% of ripgrep's files surfaced by greplm
```

## `context/pack_bench.py` — context-pack efficiency

Measures how much context a task-driven `greplm pack` delivers versus reading, in
full, every file the pack drew from. Tasks live in
[`context/tasks.json`](context/tasks.json).

```bash
python3 bench/context/pack_bench.py --budget 8000
```

Options: `--root <repo>`, `--budget <tokens>`, `--tasks <file>`, or
`--tasks-inline "how does incremental indexing work" ...`.

## Notes

- `tokens ~= characters / 4` is a rough, conservative estimate, applied
  identically to every engine so comparisons are fair.
- The binary is resolved from `$GREPLM_BIN`, then `target/release/greplm`, then
  `target/debug/greplm`, then `greplm` on `PATH` — no machine-specific paths.
