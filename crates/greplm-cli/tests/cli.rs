//! End-to-end tests for the `greplm` CLI binary.
//!
//! These spawn the actual compiled binary (via `CARGO_BIN_EXE_greplm`) against a
//! throwaway project and assert on its exit status and JSON output — the same
//! contract an agent or shell user relies on. Every query passes `--no-daemon`
//! so the tests are hermetic: they exercise the in-process path and never touch
//! (or depend on) a background daemon or socket on the host.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_greplm"))
}

/// Run greplm with `args`; return (success, stdout, stderr).
fn run(args: &[&str]) -> (bool, String, String) {
    let out = bin().args(args).output().expect("spawn greplm");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn unique_tmp(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "greplm-cli-{tag}-{}-{nanos}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn sample_project(tag: &str) -> PathBuf {
    let root = unique_tmp(tag);
    write(
        &root,
        "src/main.rs",
        "fn main() {\n    let total = compute_sum(1, 2);\n    println!(\"{}\", total);\n}\n\nfn compute_sum(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    );
    write(
        &root,
        "lib/util.py",
        "def parse_config(path):\n    return open(path).read()\n",
    );
    root
}

#[test]
fn version_and_help_succeed() {
    let (ok, out, _) = run(&["--version"]);
    assert!(ok, "--version should exit 0");
    assert!(
        out.to_lowercase().contains("greplm") || out.chars().any(|c| c.is_ascii_digit()),
        "--version output looks wrong: {out:?}"
    );

    let (ok, out, _) = run(&["--help"]);
    assert!(ok, "--help should exit 0");
    assert!(out.contains("search"), "--help should list subcommands");
}

#[test]
fn index_then_status_reports_indexed() {
    let root = sample_project("status");
    let root = root.to_str().unwrap();

    let (ok, _, err) = run(&["index", "-C", root, "--no-daemon"]);
    assert!(ok, "index should succeed; stderr={err}");

    let (ok, out, _) = run(&["status", "-C", root, "--no-daemon"]);
    assert!(ok, "status should succeed");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("status emits JSON");
    assert_eq!(v["indexed"], serde_json::Value::Bool(true));
    assert_eq!(v["doc_count"], serde_json::json!(2));
}

#[test]
fn search_json_finds_match() {
    let root = sample_project("search");
    let root = root.to_str().unwrap();
    run(&["index", "-C", root, "--no-daemon"]);

    let (ok, out, _) = run(&["search", "compute_sum", "-C", root, "--no-daemon", "--json"]);
    assert!(ok, "search should succeed");
    let hits: Vec<serde_json::Value> =
        serde_json::from_str(out.trim()).expect("search emits a JSON array");
    assert!(
        hits.iter().any(|h| h["path"] == "src/main.rs"),
        "expected a hit in src/main.rs, got {out}"
    );
}

#[test]
fn search_regex_json_finds_match() {
    let root = sample_project("regex");
    let root = root.to_str().unwrap();
    run(&["index", "-C", root, "--no-daemon"]);

    let (ok, out, _) = run(&[
        "search",
        r"fn\s+compute_\w+",
        "-C",
        root,
        "--no-daemon",
        "--regex",
        "--json",
    ]);
    assert!(ok, "regex search should succeed");
    let hits: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert!(
        hits.iter().any(|h| h["path"] == "src/main.rs"),
        "regex search should match the function definition, got {out}"
    );
}

#[test]
fn symbols_exact_json_finds_definition() {
    let root = sample_project("symbols");
    let root = root.to_str().unwrap();
    run(&["index", "-C", root, "--no-daemon"]);

    let (ok, out, _) = run(&[
        "symbols",
        "parse_config",
        "-C",
        root,
        "--no-daemon",
        "--exact",
        "--json",
    ]);
    assert!(ok, "symbols should succeed");
    let hits: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(hits.len(), 1, "exactly one parse_config, got {out}");
    assert_eq!(hits[0]["kind"], "function");
    assert_eq!(hits[0]["path"], "lib/util.py");
}

/// A query against a never-indexed project must still work: the CLI self-heals
/// by building the index on demand.
#[test]
fn search_self_heals_without_prior_index() {
    let root = unique_tmp("selfheal");
    write(&root, "src/a.rs", "fn brand_new_marker() {}\n");
    let root = root.to_str().unwrap();

    let (ok, out, _) = run(&[
        "search",
        "brand_new_marker",
        "-C",
        root,
        "--no-daemon",
        "--json",
    ]);
    assert!(ok, "search should self-heal and succeed");
    let hits: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert!(
        hits.iter().any(|h| h["path"] == "src/a.rs"),
        "self-healed search should find the marker, got {out}"
    );
}

#[test]
fn search_no_matches_emits_empty_json_array() {
    let root = sample_project("empty");
    let root = root.to_str().unwrap();
    run(&["index", "-C", root, "--no-daemon"]);

    let (ok, out, _) = run(&[
        "search",
        "zzz_definitely_absent_zzz",
        "-C",
        root,
        "--no-daemon",
        "--json",
    ]);
    assert!(ok, "search with no matches should still exit 0");
    let hits: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert!(hits.is_empty(), "expected no hits, got {out}");
}

#[test]
fn mcp_config_emits_valid_json_with_project_root() {
    let root = unique_tmp("mcp");
    let root_str = root.to_str().unwrap();

    let out = bin()
        .args(["mcp", "config", "-C", root_str, "-q"])
        .output()
        .expect("spawn greplm mcp config");
    assert!(out.status.success(), "mcp config should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid MCP JSON");
    let server = &v["mcpServers"]["greplm"];
    assert!(server["command"].as_str().unwrap().contains("greplm-mcp"));
    let args = server["args"].as_array().expect("args array");
    assert_eq!(args.len(), 1);
    assert!(
        args[0].as_str().unwrap().contains("mcp"),
        "project root should be in args: {:?}",
        args
    );
}
