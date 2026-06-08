# Contributing to greplm

Thanks for your interest in improving greplm. This guide covers the local
workflow and the checks CI enforces, so your change lands smoothly.

## Prerequisites

- **Rust 1.90+** (the project's MSRV). A stable toolchain is enough for everyday
  work; fuzzing additionally needs a nightly toolchain.
- The repository is a Cargo workspace with three crates:
  - `greplm-core` — indexing and search engine
  - `greplm-cli` — the `greplm` command-line interface
  - `greplm-mcp` — the Model Context Protocol stdio server

## Build and test

```bash
cargo build --workspace --locked
cargo test  --workspace --locked
```

The suite includes unit tests, property tests, crash-injection durability tests,
and end-to-end tests that spawn the real CLI and MCP binaries.

## Formatting and linting

CI treats warnings as errors (`RUSTFLAGS="-D warnings"`), so match it locally:

```bash
cargo fmt --all                      # apply formatting
cargo fmt --all --check              # what CI checks
cargo clippy --workspace --all-targets --locked
```

## Optional features

Two optional features are exercised in CI and should keep building cleanly:

```bash
# Offline semantic (vector) search layer.
cargo test  -p greplm-core --features semantic
cargo build -p greplm-cli  --features semantic

# Linux-only io_uring ingest backend (a no-op placeholder elsewhere).
cargo build -p greplm-core --features io-uring
```

## Fuzzing (nightly)

Coverage-guided fuzz targets live in the standalone `fuzz/` workspace (kept out of
the main workspace because it's nightly/libFuzzer-only). See
[`fuzz/README.md`](fuzz/README.md).

```bash
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run proto_request          # or any target in fuzz/fuzz_targets/
```

## Benchmarks

Criterion microbenchmarks for hot paths:

```bash
cargo bench -p greplm-core
```

CI compiles the benches on every PR (so they can't bit-rot) and runs full
measurements on a schedule via the `benchmarks` workflow.

## Coverage

```bash
cargo install cargo-llvm-cov
cargo llvm-cov --workspace --summary-only
```

CI publishes a coverage summary and an `lcov.info` artifact on every run.

## What CI enforces

Your PR must pass:

| Check | Command |
|-------|---------|
| Formatting | `cargo fmt --all --check` |
| Lints | `cargo clippy --workspace --all-targets` (warnings = errors) |
| Tests | `cargo test --workspace` on Linux, macOS, and Windows |
| MSRV | `cargo build --workspace` on Rust 1.90 |
| Benches compile | `cargo bench -p greplm-core --no-run` |
| Feature matrix | `semantic` and `io-uring` build/lint/test |
| Coverage | `cargo llvm-cov --workspace` |
| Security audit | `cargo audit` |

Fuzzing runs on a schedule and on changes to the fuzz targets.

## Commit and PR conventions

- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat:`, `fix:`, `docs:`, `chore:`, `test:`, `refactor:`, …).
- Keep PRs focused; include tests for behavior changes and bug fixes.
- Update [`CHANGELOG.md`](CHANGELOG.md) under `## [Unreleased]` for any
  user-visible change.

## Releasing

Releases are tag-driven. To cut `vX.Y.Z`:

1. Bump the version in **two** places (kept in lockstep):
   - `workspace.package.version` in the root `Cargo.toml`
   - the `greplm-core` requirement in `[workspace.dependencies]` in the root `Cargo.toml`
2. Move the `## [Unreleased]` entries into a new `## [X.Y.Z]` section in
   `CHANGELOG.md` and update the comparison links.
3. Commit, then push an annotated tag:

   ```bash
   git tag -a vX.Y.Z -m "vX.Y.Z"
   git push origin vX.Y.Z
   ```

The `release` workflow then builds binaries for all platforms, creates the GitHub
release, and publishes the crates to crates.io in dependency order
(`greplm-core` → `greplm-cli` → `greplm-mcp`).

## Reporting security issues

Please do not open public issues for vulnerabilities. See [SECURITY.md](SECURITY.md).
