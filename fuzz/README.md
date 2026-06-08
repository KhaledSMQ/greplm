# greplm fuzzing

Coverage-guided fuzz targets for greplm's parser and deserializer boundaries —
the surfaces that take adversarial input (a hostile daemon client, an arbitrary
search pattern, or a corrupt/truncated on-disk index). The goal is robustness:
these paths must return clean errors, never panic, abort, hang, or read out of
bounds.

This is a **standalone, nightly-only** workspace (libFuzzer via
[`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz)); it is intentionally
excluded from the main stable workspace.

## Targets

| Target | Surface under test |
| --- | --- |
| `proto_request` | NDJSON daemon protocol decode (`proto::Request` / `RoutedRequest`) |
| `trigram_query` | Trigram extraction + literal/regex query decomposition |
| `structural_compile` | Structural (AST) query compilation and execution |
| `symbol_extract` | tree-sitter symbol/reference extraction over arbitrary bytes |
| `segment_postings` | On-disk postings decode against a corrupt index segment |

## Running

```sh
# One-time setup.
rustup toolchain install nightly
cargo install cargo-fuzz

# Fuzz a target indefinitely.
cargo +nightly fuzz run trigram_query

# Short smoke run (what CI does on PRs).
cargo +nightly fuzz run trigram_query -- -max_total_time=60

# Reproduce / minimize a crashing input.
cargo +nightly fuzz run <target> artifacts/<target>/<crash-file>
cargo +nightly fuzz tmin  <target> artifacts/<target>/<crash-file>
```

When a target finds a crash, libFuzzer writes the input to
`artifacts/<target>/`. Add a deterministic regression test in
`crates/greplm-core` (which runs on stable in normal CI) for any real bug
found, so the fix stays pinned independent of the nightly fuzz job.
