//! Microbenchmarks for greplm's hot paths.
//!
//! These guard the performance of the engine's core loops so a refactor can't
//! silently regress them. Two layers are covered:
//!
//! * Pure CPU kernels run during indexing and query planning — trigram
//!   extraction, literal/regex trigram-query construction, and tree-sitter
//!   symbol extraction.
//! * End-to-end query execution over a real, on-disk index built once in a
//!   temp directory — literal, regex, case-insensitive, and whole-word search,
//!   plus the raw trigram candidate-filtering step.
//!
//! Run all: `cargo bench -p greplm-core`
//! Establish/compare a baseline:
//!   `cargo bench -p greplm-core -- --save-baseline main`
//!   `cargo bench -p greplm-core -- --baseline main`

use std::hint::black_box;
use std::path::{Path, PathBuf};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};

use greplm_core::lang::Language;
use greplm_core::meta::Meta;
use greplm_core::paths::Paths;
use greplm_core::search::SearchQuery;
use greplm_core::segment::Segment;
use greplm_core::symbol;
use greplm_core::trigram::{self, TrigramQuery};
use greplm_core::Greplm;

/// A realistic ~Rust source buffer for the pure-kernel benches. Repeated,
/// code-like tokens give representative trigram density and parse structure.
fn sample_source() -> String {
    let mut s = String::with_capacity(16 * 1024);
    for i in 0..200 {
        s.push_str(&format!(
            "/// Documentation for item {i} describing the shared_token contract.\n\
             pub fn handler_{i}(request: &Request, ctx: &mut Context) -> Result<Response> {{\n\
             \x20\x20\x20\x20let value_{i} = compute_value(request, {i});\n\
             \x20\x20\x20\x20let parsed = parse_config(&ctx.config, value_{i})?;\n\
             \x20\x20\x20\x20shared_token(parsed).map(|v| Response::new(v))\n\
             }}\n\n\
             struct State_{i} {{ count: usize, name: String, nodes: Vec<Node> }}\n\n"
        ));
    }
    s
}

fn bench_kernels(c: &mut Criterion) {
    let source = sample_source();
    let bytes = source.as_bytes();

    let mut group = c.benchmark_group("kernels");
    group.throughput(Throughput::Bytes(bytes.len() as u64));

    group.bench_function("trigram_extract", |b| {
        b.iter(|| black_box(trigram::extract(black_box(bytes))))
    });

    group.bench_function("symbol_extract_all_rust", |b| {
        b.iter(|| black_box(symbol::extract_all(Language::Rust, black_box(bytes))))
    });

    group.finish();

    // Query-planning kernels operate on the pattern, not the corpus, so they're
    // tiny — measure them without throughput.
    let mut group = c.benchmark_group("query_plan");
    group.bench_function("from_literal", |b| {
        b.iter(|| black_box(TrigramQuery::from_literal(black_box(b"compute_value"))))
    });
    group.bench_function("from_literal_ci", |b| {
        b.iter(|| black_box(TrigramQuery::from_literal_ci(black_box(b"compute_value"))))
    });
    group.bench_function("regex_trigrams", |b| {
        b.iter(|| {
            black_box(trigram::regex_trigrams(
                black_box(r"handler_\d+\(request"),
                false,
            ))
        })
    });
    group.finish();
}

/// An indexed temp project, torn down on drop.
struct Corpus {
    g: Greplm,
    root: PathBuf,
}

impl Drop for Corpus {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
    }
}

fn build_corpus(file_count: usize, lines_per_file: usize) -> Corpus {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut root = std::env::temp_dir();
    root.push(format!("greplm-bench-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();

    for f in 0..file_count {
        let mut body = String::with_capacity(lines_per_file * 48);
        for l in 0..lines_per_file {
            // A small, repeating vocabulary so trigram postings have realistic
            // overlap; "shared_token" appears in a predictable fraction of lines.
            if l % 7 == 0 {
                body.push_str(&format!("    let r{l} = shared_token(value_{f});\n"));
            } else if l % 3 == 0 {
                body.push_str(&format!(
                    "    fn helper_{f}_{l}(node: &Node) {{ compute_value(node); }}\n"
                ));
            } else {
                body.push_str(&format!("    process_data(buffer_{f}, index_{l});\n"));
            }
        }
        write(&root, &format!("src/module_{f}/file_{f}.rs"), &body);
    }

    let g = Greplm::open(&root).unwrap();
    g.index(true).unwrap();
    Corpus { g, root }
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn bench_search(c: &mut Criterion) {
    let corpus = build_corpus(150, 80);
    let searcher = corpus.g.searcher().unwrap();

    let mut group = c.benchmark_group("search");

    // A common literal present in many files (exercises candidate filtering +
    // verification across many docs).
    group.bench_function("literal_common", |b| {
        b.iter(|| {
            black_box(
                searcher
                    .search(black_box(&SearchQuery {
                        pattern: "shared_token".to_string(),
                        ..Default::default()
                    }))
                    .unwrap(),
            )
        })
    });

    // A rare literal (most candidates pruned by trigram intersection).
    group.bench_function("literal_rare", |b| {
        b.iter(|| {
            black_box(
                searcher
                    .search(black_box(&SearchQuery {
                        pattern: "helper_3_9".to_string(),
                        ..Default::default()
                    }))
                    .unwrap(),
            )
        })
    });

    group.bench_function("regex", |b| {
        b.iter(|| {
            black_box(
                searcher
                    .search(black_box(&SearchQuery {
                        pattern: r"compute_value\(node\)".to_string(),
                        regex: true,
                        ..Default::default()
                    }))
                    .unwrap(),
            )
        })
    });

    group.bench_function("case_insensitive", |b| {
        b.iter(|| {
            black_box(
                searcher
                    .search(black_box(&SearchQuery {
                        pattern: "SHARED_TOKEN".to_string(),
                        case_insensitive: true,
                        ..Default::default()
                    }))
                    .unwrap(),
            )
        })
    });

    group.bench_function("whole_word", |b| {
        b.iter(|| {
            black_box(
                searcher
                    .search(black_box(&SearchQuery {
                        pattern: "shared_token".to_string(),
                        whole_word: true,
                        ..Default::default()
                    }))
                    .unwrap(),
            )
        })
    });

    group.finish();
}

fn bench_candidates(c: &mut Criterion) {
    let corpus = build_corpus(150, 80);
    let paths = Paths::new(&corpus.root);
    let meta = Meta::load(&paths.meta_file()).unwrap();
    let seg_id = *meta.segments.first().expect("one segment");
    let seg = Segment::open(&paths, seg_id).unwrap();

    let common = TrigramQuery::from_literal(b"shared_token");
    let rare = TrigramQuery::from_literal(b"helper_3_9");

    let mut group = c.benchmark_group("candidates");
    group.bench_function("common", |b| {
        b.iter(|| black_box(seg.candidates(black_box(&common)).unwrap()))
    });
    group.bench_function("rare", |b| {
        b.iter(|| black_box(seg.candidates(black_box(&rare)).unwrap()))
    });
    group.finish();
}

criterion_group!(benches, bench_kernels, bench_search, bench_candidates);
criterion_main!(benches);
