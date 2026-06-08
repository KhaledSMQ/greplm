# Token efficiency

greplm exists to keep coding agents off the "grep, then read whole files" treadmill
that burns context. Every query returns compact locations (and, for `snippet`, an exact
slice) instead of file bodies, so the agent pulls in a few lines rather than thousands.

greplm tracks this automatically. Each query records the grep+read baseline (the full
size of the unique files it referenced) against the size of the payload it actually
returned; `greplm savings` aggregates the estimate (≈4 chars/token, a conservative basis):

```bash
greplm savings            # rolling 24h / 7d / all-time summary
greplm savings --verbose  # also break down by query kind
greplm savings --json     # machine-readable
```

```
  greplm Token Savings
  ================================================================
  Period          Calls   Savings
  ----------------------------------------------------------------
  Last 24h            4   [███████████████░]  ~95.6k tokens (96%)
  Last 7 days         4   [███████████████░]  ~95.6k tokens (96%)
  All time            4   [███████████████░]  ~95.6k tokens (96%)
```

Stats live in `.greplm/savings.jsonl`; set `GREPLM_NO_SAVINGS=1` to disable recording.

## Benchmarks

To reproduce the efficiency numbers, run the benchmark in [`bench/`](../bench/). It runs
against this repository itself and needs only a release build plus `ripgrep` — no
external corpus, embedding model, or third-party tool:

```bash
cargo build --release

# Search efficiency vs the ripgrep + read-whole-files baseline:
python3 bench/run_bench.py

# Context-pack efficiency (budgeted packs vs reading whole files):
python3 bench/context/pack_bench.py
```

A typical run on this repo shows greplm returning the same files as ripgrep with
**~99% fewer tokens** for content search and **~89% fewer** for context packs. See
[`bench/README.md`](../bench/README.md) for the methodology.
