//! End-to-end test: build an index in a temp directory and query it.

use std::path::{Path, PathBuf};

use greplm_core::meta::Meta;
use greplm_core::paths::Paths;
use greplm_core::search::{SearchQuery, SymbolQuery};
use greplm_core::Greplm;

fn temp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("greplm-test-{tag}-{nanos}"));
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

#[test]
fn index_search_symbols_and_incremental() {
    let root = temp_dir("e2e");

    write(
        &root,
        "src/main.rs",
        "fn main() {\n    let total = compute_sum(1, 2);\n    println!(\"{}\", total);\n}\n\nfn compute_sum(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    );
    write(
        &root,
        "lib/util.py",
        "def parse_config(path):\n    return open(path).read()\n\nclass Loader:\n    def load(self):\n        return parse_config('x')\n",
    );

    let g = Greplm::open(&root).unwrap();
    let stats = g.index(true).unwrap();
    assert_eq!(stats.files_indexed, 2, "should index two files");
    assert!(stats.symbols >= 4, "should find several symbols");

    let searcher = g.searcher().unwrap();

    // Literal content search.
    let hits = searcher
        .search(&SearchQuery {
            pattern: "compute_sum".to_string(),
            ..Default::default()
        })
        .unwrap();
    assert!(
        hits.iter().any(|h| h.path == "src/main.rs"),
        "compute_sum should be found in main.rs"
    );

    // The definition line should rank at the top (symbol boost).
    assert_eq!(hits[0].line, 6, "definition line ranks first");

    // Regex search.
    let rx = searcher
        .search(&SearchQuery {
            pattern: r"fn\s+compute_\w+".to_string(),
            regex: true,
            ..Default::default()
        })
        .unwrap();
    assert!(!rx.is_empty(), "regex should match the function definition");

    // Symbol lookup (exact + fuzzy).
    let syms = searcher
        .symbols(&SymbolQuery {
            name: "parse_config".to_string(),
            exact: true,
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(syms.len(), 1);
    assert_eq!(syms[0].kind, "function");
    assert_eq!(syms[0].path, "lib/util.py");

    // Outline.
    let outline = searcher.outline("lib/util.py").unwrap();
    assert!(outline
        .iter()
        .any(|s| s.name == "Loader" && s.kind == "class"));

    // Incremental update: add a new file, reindex, and find new content.
    write(&root, "src/extra.rs", "pub fn brand_new_marker() {}\n");
    let stats2 = g.index(false).unwrap();
    assert_eq!(stats2.files_indexed, 1, "only the new file is reindexed");

    let searcher2 = g.searcher().unwrap();
    let new_hits = searcher2
        .search(&SearchQuery {
            pattern: "brand_new_marker".to_string(),
            ..Default::default()
        })
        .unwrap();
    assert!(
        new_hits.iter().any(|h| h.path == "src/extra.rs"),
        "incremental index should find new content"
    );

    // Deletion is reflected via tombstones.
    std::fs::remove_file(root.join("src/extra.rs")).unwrap();
    let stats3 = g.index(false).unwrap();
    assert_eq!(stats3.files_removed, 1, "deleted file is tombstoned");
    let searcher3 = g.searcher().unwrap();
    let gone = searcher3
        .search(&SearchQuery {
            pattern: "brand_new_marker".to_string(),
            ..Default::default()
        })
        .unwrap();
    assert!(gone.is_empty(), "deleted content should no longer match");

    std::fs::remove_dir_all(&root).ok();
}

/// Upgrading to a greplm whose on-disk schema bumped should not wedge an
/// already-indexed project: a plain incremental `index` must detect the stale
/// manifest and transparently rebuild from scratch instead of erroring.
#[test]
fn schema_bump_triggers_automatic_rebuild() {
    let root = temp_dir("schema-bump");
    write(&root, "src/main.rs", "fn alpha_marker() {}\n");

    let g = Greplm::open(&root).unwrap();
    g.index(true).unwrap();

    // Simulate an upgrade that bumped the on-disk format: stamp the manifest
    // with a schema version this build no longer supports.
    let paths = Paths::new(&root);
    let mut meta = Meta::load(&paths.meta_file()).unwrap();
    meta.schema_version = u32::MAX;
    meta.save(&paths.meta_file()).unwrap();

    // Opening a searcher against the outdated manifest must fail loudly...
    assert!(
        g.searcher().is_err(),
        "searcher should reject an unsupported schema version"
    );

    // ...but a normal (non-forced) index call should recover automatically.
    let stats = g.index(false).unwrap();
    assert_eq!(stats.files_indexed, 1, "rebuild should reindex the file");

    let searcher = g.searcher().unwrap();
    let hits = searcher
        .search(&SearchQuery {
            pattern: "alpha_marker".to_string(),
            ..Default::default()
        })
        .unwrap();
    assert!(
        hits.iter().any(|h| h.path == "src/main.rs"),
        "search should work again after the automatic rebuild"
    );

    // The manifest should be back on the current supported schema version.
    assert!(
        Meta::load(&paths.meta_file()).is_ok(),
        "rebuilt manifest should load cleanly"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// The code-intelligence layer: call graph (callers/callees), blast radius,
/// typed go-to-definition, resolved references, structural search, and context
/// packs — all over a small indexed tree.
#[test]
fn code_intelligence_graph_def_ast_pack() {
    let root = temp_dir("codeintel");
    write(
        &root,
        "src/main.rs",
        "fn helper() -> i32 {\n    40 + 2\n}\n\nfn main() {\n    let x = helper();\n    println!(\"{}\", x);\n}\n",
    );

    let g = Greplm::open(&root).unwrap();
    g.index(true).unwrap();
    let s = g.searcher().unwrap();

    // callers(helper) -> main calls it.
    let callers = s.callers("helper", 50, 0);
    assert!(
        callers
            .iter()
            .any(|c| c.caller.as_deref() == Some("main") && c.callee == "helper"),
        "main should be a caller of helper, got {callers:?}"
    );

    // callees(main) -> includes helper.
    let callees = s.callees("main", 50, 0);
    assert!(
        callees.iter().any(|c| c.callee == "helper"),
        "main should call helper, got {callees:?}"
    );

    // blast_radius(helper) -> main is at distance 1.
    let impact = s.blast_radius("helper", 3, 50);
    assert!(
        impact.iter().any(|n| n.name == "helper" && n.distance == 0),
        "target itself at distance 0"
    );
    assert!(
        impact.iter().any(|n| n.name == "main" && n.distance == 1),
        "main affected at distance 1, got {impact:?}"
    );

    // Typed go-to-definition at the `helper()` call site (line 6, col 13).
    let defs = s.definition("src/main.rs", 6, 13).unwrap();
    assert!(
        defs.iter()
            .any(|d| d.name == "helper" && d.line_start == 1 && d.resolved),
        "def should resolve helper to line 1, got {defs:?}"
    );

    // references_resolved: a definition plus a call site.
    let xref = s.references_resolved("helper", 50, 0);
    assert!(xref.iter().any(|r| r.kind == "definition"));
    assert!(xref.iter().any(|r| r.kind == "call"));

    // Structural search: find call expressions.
    let st = s
        .structural_search(
            "(call_expression function: (identifier) @fn)",
            "rust",
            50,
            0,
        )
        .unwrap();
    assert!(
        st.iter()
            .flat_map(|m| m.captures.iter())
            .any(|c| c.text == "helper"),
        "structural search should capture the helper call, got {st:?}"
    );

    // Context pack: a budget-bounded bundle that surfaces helper.
    let pack = s.context_pack("helper that computes a sum", 4000);
    assert!(
        pack.items.iter().any(|i| i.name == "helper"),
        "context pack should include helper, got {:?}",
        pack.items.iter().map(|i| &i.name).collect::<Vec<_>>()
    );
    assert!(pack.used_tokens <= pack.budget_tokens.max(1) + 64);

    std::fs::remove_dir_all(&root).ok();
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_run(root: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?} failed");
}

/// Git time-travel: blame, symbol history, and changed-since over a real repo.
#[test]
fn git_blame_history_changed() {
    if !git_available() {
        eprintln!("git not available; skipping git_blame_history_changed");
        return;
    }
    let root = temp_dir("git");
    git_run(&root, &["init", "-q"]);
    git_run(&root, &["config", "user.email", "t@t.t"]);
    git_run(&root, &["config", "user.name", "tester"]);

    write(&root, "src/main.rs", "fn helper() {\n    let v = 1;\n}\n");
    git_run(&root, &["add", "-A"]);
    git_run(&root, &["commit", "-qm", "initial"]);

    write(
        &root,
        "src/main.rs",
        "fn helper() {\n    let v = 2; // changed\n}\n",
    );
    git_run(&root, &["add", "-A"]);
    git_run(&root, &["commit", "-qm", "tweak helper"]);

    let g = Greplm::open(&root).unwrap();
    g.index(true).unwrap();
    let s = g.searcher().unwrap();

    // Blame the changed line.
    let b = s.blame("src/main.rs", 2).unwrap();
    assert_eq!(b.author, "tester");
    assert_eq!(b.summary, "tweak helper");

    // Symbol history resolves helper and lists both commits.
    let h = s.symbol_history("helper", 20).unwrap();
    assert_eq!(h.name, "helper");
    assert!(
        h.commits.len() >= 2,
        "expected >=2 commits, got {:?}",
        h.commits
    );

    // changed_since the first commit reports main.rs and its symbols.
    let changed = s.changed_since("HEAD~1").unwrap();
    let entry = changed.iter().find(|c| c.path == "src/main.rs");
    assert!(entry.is_some(), "main.rs should be reported changed");
    assert!(
        entry.unwrap().symbols.iter().any(|n| n == "helper"),
        "changed entry should list the helper symbol"
    );

    // Meta records the indexed HEAD for branch-switch detection.
    let meta = Meta::load(&Paths::new(&root).meta_file()).unwrap();
    assert!(!meta.indexed_git_head.is_empty(), "HEAD should be recorded");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn case_insensitive_multiline_and_compaction() {
    let root = temp_dir("ci");

    write(
        &root,
        "src/main.rs",
        "fn ComputeSum() {}\n// also COMPUTESUM referenced\nstruct Wrapper;\n",
    );
    // A file with a multi-line construct for regex span testing.
    write(
        &root,
        "src/multi.rs",
        "fn header(\n    arg: i32,\n) -> i32 { arg }\n",
    );

    let g = Greplm::open(&root).unwrap();
    g.index(true).unwrap();
    let searcher = g.searcher().unwrap();

    // Case-insensitive literal now uses the trigram index and still matches all
    // case variants.
    let ci = searcher
        .search(&SearchQuery {
            pattern: "computesum".to_string(),
            case_insensitive: true,
            ..Default::default()
        })
        .unwrap();
    assert!(
        ci.iter().filter(|h| h.path == "src/main.rs").count() >= 2,
        "case-insensitive search should match both ComputeSum and COMPUTESUM"
    );

    // Non-ASCII case-insensitive: the file holds the uppercase form ("CAFÉRUNNER",
    // where 'É' differs in bytes from 'é'). The Unicode-aware matcher folds
    // é<->É, so the lowercase query must still find it. This regressed before the
    // trigram filter learned to skip windows it can't ASCII-fold soundly.
    write(&root, "src/accent.rs", "// CAFÉRUNNER marker\nfn z() {}\n");
    g.index(false).unwrap();
    let searcher = g.searcher().unwrap();
    let accent = searcher
        .search(&SearchQuery {
            pattern: "caférunner".to_string(),
            case_insensitive: true,
            ..Default::default()
        })
        .unwrap();
    assert!(
        accent.iter().any(|h| h.path == "src/accent.rs"),
        "case-insensitive search must find the non-ASCII uppercase form"
    );

    // Multiline regex: the pattern spans a newline between `header(` and `arg`.
    let ml = searcher
        .search(&SearchQuery {
            pattern: r"header\(\s*\n\s*arg".to_string(),
            regex: true,
            ..Default::default()
        })
        .unwrap();
    assert!(
        ml.iter().any(|h| h.path == "src/multi.rs"),
        "regex should match across line boundaries"
    );

    // Compaction via several incremental updates to accumulate segments, then an
    // explicit reindex path is exercised through the public force rebuild which
    // shares the merge code via fallback; here we trigger merge directly.
    for i in 0..3 {
        write(
            &root,
            &format!("src/gen{i}.rs"),
            &format!("fn gen{i}() {{}}\n"),
        );
        g.index(false).unwrap();
    }
    let stats = g.compact().unwrap();
    assert_eq!(stats.segments, 1, "compaction merges into a single segment");

    // Index remains correct and complete after compaction.
    let searcher = g.searcher().unwrap();
    for i in 0..3 {
        let hits = searcher
            .search(&SearchQuery {
                pattern: format!("gen{i}"),
                ..Default::default()
            })
            .unwrap();
        assert!(
            hits.iter().any(|h| h.path == format!("src/gen{i}.rs")),
            "gen{i} should still be findable after compaction"
        );
    }
    // A subsequent incremental update still works against the merged segment.
    write(&root, "src/post.rs", "fn after_compaction() {}\n");
    let s = g.index(false).unwrap();
    assert_eq!(s.files_indexed, 1);
    let searcher = g.searcher().unwrap();
    let hits = searcher
        .search(&SearchQuery {
            pattern: "after_compaction".to_string(),
            ..Default::default()
        })
        .unwrap();
    assert!(hits.iter().any(|h| h.path == "src/post.rs"));

    std::fs::remove_dir_all(&root).ok();
}

/// `read_snippet` must clamp out-of-range line numbers and never overflow when
/// `end_line + context` would exceed `u32::MAX`.
#[test]
fn read_snippet_clamps_extreme_ranges() {
    let root = temp_dir("snippet");
    write(&root, "src/small.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");

    let g = Greplm::open(&root).unwrap();
    g.index(true).unwrap();
    let searcher = g.searcher().unwrap();

    // Enormous end_line and context: previously `end_line + context` could
    // overflow; now both clamp to the file's 3 lines without panicking.
    let snip = searcher
        .read_snippet("src/small.rs", 1, u32::MAX, u32::MAX)
        .unwrap();
    assert_eq!(snip.total_lines, 3);
    assert_eq!(snip.start_line, 1);
    assert_eq!(snip.end_line, 3);
    assert_eq!(snip.lines.len(), 3);

    // A start line past EOF must clamp `start_line` into the file rather than
    // reporting an inverted range (start_line > end_line/total_lines).
    let past_eof = searcher
        .read_snippet("src/small.rs", 999_999, 1_000_005, 3)
        .unwrap();
    assert_eq!(past_eof.total_lines, 3);
    assert!(past_eof.start_line <= past_eof.end_line);
    assert!(past_eof.start_line <= past_eof.total_lines);
    assert_eq!(past_eof.end_line, 3);

    std::fs::remove_dir_all(&root).ok();
}

/// `read_snippet` must refuse to read outside the project root: parent-dir
/// traversal and absolute paths both have to be rejected, while a normal
/// in-repo path still works.
#[test]
fn read_snippet_rejects_path_traversal() {
    let root = temp_dir("traversal");
    write(&root, "src/small.rs", "fn a() {}\n");
    // A secret living just outside the indexed project root.
    let secret = root.parent().unwrap().join("greplm-secret.txt");
    std::fs::write(&secret, "TOP SECRET").unwrap();

    let g = Greplm::open(&root).unwrap();
    g.index(true).unwrap();
    let searcher = g.searcher().unwrap();

    // Relative traversal out of the root must fail.
    assert!(
        searcher
            .read_snippet("../greplm-secret.txt", 1, 1, 0)
            .is_err(),
        "parent-dir traversal must be rejected"
    );

    // An absolute path (which `Path::join` would otherwise honor wholesale)
    // must fail.
    assert!(
        searcher
            .read_snippet(secret.to_str().unwrap(), 1, 1, 0)
            .is_err(),
        "absolute path must be rejected"
    );

    // A legitimate in-repo path still works.
    assert!(searcher.read_snippet("src/small.rs", 1, 1, 0).is_ok());

    std::fs::remove_file(&secret).ok();
    std::fs::remove_dir_all(&root).ok();
}

/// A regex that can only match zero width (e.g. `z*` over text without `z`)
/// must not flag every line; zero-width matches are skipped.
#[test]
fn regex_zero_width_matches_are_skipped() {
    let root = temp_dir("zerowidth");
    write(&root, "src/a.rs", "alpha beta\ngamma delta\nepsilon\n");

    let g = Greplm::open(&root).unwrap();
    g.index(true).unwrap();
    let searcher = g.searcher().unwrap();

    let hits = searcher
        .search(&SearchQuery {
            pattern: "z*".to_string(),
            regex: true,
            ..Default::default()
        })
        .unwrap();
    assert!(
        hits.is_empty(),
        "zero-width-only regex should produce no hits, got {hits:?}"
    );

    // A real (non-empty) match in the same file is still found.
    let real = searcher
        .search(&SearchQuery {
            pattern: "gam*".to_string(),
            regex: true,
            ..Default::default()
        })
        .unwrap();
    assert!(real.iter().any(|h| h.line == 2), "real match still found");

    std::fs::remove_dir_all(&root).ok();
}

/// Incrementally maintained `doc_count` / `symbol_count` must stay exactly equal
/// to what a full rebuild of the same tree produces, across add/modify/delete.
#[test]
fn incremental_counts_match_full_rebuild() {
    let root = temp_dir("counts");
    write(&root, "src/a.rs", "fn one() {}\nfn two() {}\n");
    write(
        &root,
        "lib/b.py",
        "def parse():\n    return 1\n\nclass Loader:\n    def load(self):\n        return 2\n",
    );

    let g = Greplm::open(&root).unwrap();
    g.index(true).unwrap();

    // Modify a.rs (add a symbol), add a new file, delete b.py — all incrementally.
    write(
        &root,
        "src/a.rs",
        "fn one() {}\nfn two() {}\nfn three() {}\n",
    );
    write(&root, "src/c.rs", "fn fresh() {}\n");
    std::fs::remove_file(root.join("lib/b.py")).unwrap();
    g.index(false).unwrap();

    let incremental = g.status().unwrap();

    // Ground truth: rebuild the identical tree from scratch.
    let full = g.index(true).unwrap();
    let rebuilt = g.status().unwrap();

    assert_eq!(
        incremental.doc_count, rebuilt.doc_count,
        "incremental doc_count must match full rebuild"
    );
    assert_eq!(
        incremental.symbol_count, rebuilt.symbol_count,
        "incremental symbol_count must match full rebuild"
    );
    assert_eq!(incremental.doc_count, 2, "two files remain after delete");
    assert_eq!(full.files_indexed, 2);

    std::fs::remove_dir_all(&root).ok();
}

/// An interrupted compaction can leave the manifest referencing a different set
/// of segments than the cache. The next incremental must detect this and safely
/// rebuild rather than corrupt the index.
#[test]
fn interrupted_compaction_is_recovered() {
    let root = temp_dir("recover");
    write(&root, "src/a.rs", "fn alpha_marker() {}\n");

    let g = Greplm::open(&root).unwrap();
    g.index(true).unwrap();

    // Add a second file incrementally so the cache references two segments.
    write(&root, "src/b.rs", "fn beta_marker() {}\n");
    g.index(false).unwrap();

    // Simulate a compaction that published a manifest the cache disagrees with:
    // drop a segment from meta.json while the cache still points at it.
    let paths = Paths::new(&root);
    let meta_path = paths.meta_file();
    let mut meta = Meta::load(&meta_path).unwrap();
    assert!(meta.segments.len() >= 2, "expected multiple segments");
    meta.segments.remove(0);
    meta.save(&meta_path).unwrap();

    // The guard must notice the cache/manifest mismatch and full-rebuild.
    g.index(false).unwrap();

    let searcher = g.searcher().unwrap();
    for marker in ["alpha_marker", "beta_marker"] {
        let hits = searcher
            .search(&SearchQuery {
                pattern: marker.to_string(),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !hits.is_empty(),
            "{marker} should be findable after recovery"
        );
    }
    assert_eq!(g.status().unwrap().doc_count, 2, "both files indexed");

    std::fs::remove_dir_all(&root).ok();
}
