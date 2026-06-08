//! Property test: trigram-accelerated search must be *complete*.
//!
//! greplm answers a content query in two stages: a trigram index prunes the
//! corpus down to candidate documents, then the real literal/regex matcher
//! verifies each candidate. The danger of any such filter is a *false
//! negative* — pruning away a document that actually matches. For a search
//! tool that silently loses results, that is the single worst class of bug.
//!
//! This test pins the core correctness invariant:
//!
//! > For any corpus and any query, the indexed search returns exactly the same
//! > matches as a brute-force scan of the working tree.
//!
//! The brute-force oracle is [`grep_walk`], which reads every file and runs the
//! identical [`Matcher`](greplm_core::search) — no trigram filter. Both paths
//! share match collection, per-line de-duplication, and the exhaustive
//! `(path, line, column)` ordering, so the *only* thing under test is whether
//! trigram candidate filtering ever drops a real match. We compare in
//! `exhaustive` mode so neither ranking, pagination, nor per-file caps can mask
//! a difference: the result vectors must be byte-for-byte equal.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use greplm_core::paths::Paths;
use greplm_core::search::{grep_walk, SearchHit, SearchQuery};
use greplm_core::Greplm;
use proptest::prelude::*;

/// Relative paths for generated files. Distinct directories and extensions
/// exercise path scoring and per-language detection. Indexed at most as many
/// files as the generator produces (capped at this length).
const FILE_NAMES: &[&str] = &[
    "src/main.rs",
    "lib/util.py",
    "app/handler.js",
    "pkg/server.go",
    "notes/readme.txt",
];

/// A small vocabulary whose members deliberately share trigrams (e.g. `parse`
/// / `parser`, `value` / `values`, `compute` / `computed`) so candidate
/// filtering is exercised with overlapping postings rather than trivially
/// disjoint terms.
const TOKENS: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "compute", "computed", "sum", "value", "values", "parse",
    "parser", "helper", "main", "node", "index", "query", "trigram", "loop", "data", "handler",
    "load", "loader", "config", "server", "client", "token", "tokens", "fn", "def", "class",
];

fn unique_tmp() -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("greplm-prop-{}-{nanos}-{n}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write_file(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// The comparison key: the exact location of a match. Score and snippet text
/// are derived deterministically from these on both paths, so location
/// equality is the property that matters (and is the one a filter bug breaks).
fn keys(hits: &[SearchHit]) -> Vec<(String, u32, u32)> {
    hits.iter()
        .map(|h| (h.path.clone(), h.line, h.column))
        .collect()
}

fn token() -> impl Strategy<Value = String> {
    prop_oneof![
        // Known vocabulary (frequent real matches, shared trigrams).
        proptest::sample::select(TOKENS).prop_map(|s| s.to_string()),
        // Arbitrary short lowercase runs (sub-trigram and rare terms).
        "[a-z]{1,6}",
    ]
}

/// A query string paired with whether it should be treated as a regex. Covers
/// short and long literals plus the regex shapes whose trigram extraction is
/// most error-prone: alternation, suffix wildcards, and a trailing wildcard.
fn pattern() -> impl Strategy<Value = (String, bool)> {
    prop_oneof![
        token().prop_map(|t| (t, false)),
        (token(), token()).prop_map(|(a, b)| (format!("{a}{b}"), false)),
        "[a-z]{1,3}".prop_map(|s| (s, false)),
        (token(), token()).prop_map(|(a, b)| (format!("{a}|{b}"), true)),
        token().prop_map(|t| (format!("{t}\\w*"), true)),
        token().prop_map(|t| (format!("{t}."), true)),
    ]
}

fn line() -> impl Strategy<Value = String> {
    prop::collection::vec(token(), 1..=5).prop_map(|ts| ts.join(" "))
}

fn file_content() -> impl Strategy<Value = String> {
    prop::collection::vec(line(), 1..=8).prop_map(|lines| {
        let mut s = lines.join("\n");
        s.push('\n');
        s
    })
}

proptest! {
    // Each case builds a real on-disk index, so keep the count modest but
    // meaningful. Failures shrink to a minimal corpus + query.
    #![proptest_config(ProptestConfig {
        cases: 64,
        max_shrink_iters: 256,
        ..ProptestConfig::default()
    })]

    /// The headline invariant: indexed (trigram-filtered) search == brute-force
    /// scan, exhaustively, for literal / case-insensitive / whole-word / regex
    /// queries over a randomly generated corpus.
    #[test]
    fn indexed_search_equals_bruteforce(
        contents in prop::collection::vec(file_content(), 1..=FILE_NAMES.len()),
        (pattern, regex) in pattern(),
        case_insensitive in any::<bool>(),
        whole_word in any::<bool>(),
    ) {
        let root = unique_tmp();
        for (i, body) in contents.iter().enumerate() {
            write_file(&root, FILE_NAMES[i], body);
        }

        let g = Greplm::open(&root).unwrap();
        g.index(true).unwrap();
        let searcher = g.searcher().unwrap();

        let query = SearchQuery {
            pattern: pattern.clone(),
            regex,
            case_insensitive,
            whole_word,
            exhaustive: true,
            ..Default::default()
        };

        let indexed = searcher.search(&query).unwrap();
        let brute = grep_walk(&Paths::new(&root), g.config(), &query).unwrap();

        let indexed_keys = keys(&indexed);
        let brute_keys = keys(&brute);

        std::fs::remove_dir_all(&root).ok();

        prop_assert_eq!(
            indexed_keys,
            brute_keys,
            "trigram search dropped or invented matches for pattern={:?} regex={} ci={} ww={}",
            pattern, regex, case_insensitive, whole_word
        );
    }

    /// Repeating the same query against the same index is deterministic: the
    /// parallel verify stage must not reorder or drop results run-to-run.
    #[test]
    fn search_is_deterministic(
        contents in prop::collection::vec(file_content(), 1..=FILE_NAMES.len()),
        (pattern, regex) in pattern(),
    ) {
        let root = unique_tmp();
        for (i, body) in contents.iter().enumerate() {
            write_file(&root, FILE_NAMES[i], body);
        }

        let g = Greplm::open(&root).unwrap();
        g.index(true).unwrap();
        let searcher = g.searcher().unwrap();

        let query = SearchQuery {
            pattern: pattern.clone(),
            regex,
            exhaustive: true,
            ..Default::default()
        };

        let first = keys(&searcher.search(&query).unwrap());
        let second = keys(&searcher.search(&query).unwrap());

        std::fs::remove_dir_all(&root).ok();

        prop_assert_eq!(first, second, "search results differed across identical runs");
    }
}

/// A fixed, human-readable sanity check independent of the random generators:
/// it guards the harness itself (oracle wiring, exhaustive comparison) so a
/// proptest failure can be trusted to indicate a real engine bug rather than a
/// broken test fixture.
#[test]
fn harness_self_check_known_corpus() {
    let root = unique_tmp();
    write_file(
        &root,
        "src/main.rs",
        "fn compute_sum() {}\nlet computed = compute_sum();\n// COMPUTE marker\n",
    );
    write_file(
        &root,
        "lib/util.py",
        "def parse_config():\n    return parse()\n",
    );

    let g = Greplm::open(&root).unwrap();
    g.index(true).unwrap();
    let searcher = g.searcher().unwrap();

    for query in [
        SearchQuery {
            pattern: "compute".to_string(),
            exhaustive: true,
            ..Default::default()
        },
        SearchQuery {
            pattern: "compute".to_string(),
            case_insensitive: true,
            exhaustive: true,
            ..Default::default()
        },
        SearchQuery {
            pattern: "parse".to_string(),
            whole_word: true,
            exhaustive: true,
            ..Default::default()
        },
        SearchQuery {
            pattern: r"comp\w+".to_string(),
            regex: true,
            exhaustive: true,
            ..Default::default()
        },
    ] {
        let indexed = keys(&searcher.search(&query).unwrap());
        let brute = keys(&grep_walk(&Paths::new(&root), g.config(), &query).unwrap());
        assert_eq!(indexed, brute, "mismatch for {query:?}");
    }

    // And at least one of those queries actually matched, so the oracle isn't
    // trivially agreeing on "no results".
    let hits = searcher
        .search(&SearchQuery {
            pattern: "compute".to_string(),
            exhaustive: true,
            ..Default::default()
        })
        .unwrap();
    assert!(
        !hits.is_empty(),
        "expected real matches in the known corpus"
    );

    std::fs::remove_dir_all(&root).ok();
}
