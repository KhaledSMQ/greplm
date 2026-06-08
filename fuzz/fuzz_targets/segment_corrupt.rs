//! Fuzz every on-disk segment component against corruption.
//!
//! A `.greplm` index is untrusted in practice: any file can be truncated,
//! corrupted, or partially written. Opening and querying must degrade to a
//! clean `Err` (triggering the self-healing rebuild path), never a panic.
//!
//! Strategy: build one valid segment once, snapshot every file, then on each
//! input overwrite a single selected file with fuzzer bytes, exercise open/query
//! paths, and restore the snapshot so the next iteration starts from a good index.
#![no_main]

use std::path::PathBuf;
use std::sync::OnceLock;

use greplm_core::meta::Meta;
use greplm_core::paths::Paths;
use greplm_core::search::{SearchQuery, Searcher, SymbolQuery};
use greplm_core::segment::Segment;
use greplm_core::trigram::TrigramQuery;
use greplm_core::Greplm;
use libfuzzer_sys::fuzz_target;

const NEEDLES: &[&[u8]] = &[b"abc", b"fn ", b"let", b"call"];

struct SegmentFiles {
    fst: PathBuf,
    post: PathBuf,
    docs: PathBuf,
    syms: PathBuf,
    refs: PathBuf,
    live: PathBuf,
}

struct ValidSegment {
    fst: Vec<u8>,
    post: Vec<u8>,
    docs: Vec<u8>,
    syms: Vec<u8>,
    refs: Vec<u8>,
    live: Vec<u8>,
    meta: Vec<u8>,
}

struct Fixture {
    paths: Paths,
    seg_id: u64,
    files: SegmentFiles,
    meta_file: PathBuf,
    valid: ValidSegment,
}

fn read_file(path: &PathBuf) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let mut dir = std::env::temp_dir();
        dir.push(format!("greplm-fuzz-seg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create fuzz dir");

        std::fs::write(
            dir.join("sample.rs"),
            b"fn abcabc() {\n    let abcdef = 123;\n    call_abc(abcdef);\n}\n",
        )
        .expect("write sample");

        let g = Greplm::open(&dir).expect("open project");
        g.index(true).expect("build index");

        let paths = Paths::new(&dir);
        let meta_file = paths.meta_file();
        let meta = Meta::load(&meta_file).expect("load meta");
        let seg_id = *meta.segments.first().expect("at least one segment");

        let files = SegmentFiles {
            fst: paths.fst_file(seg_id),
            post: paths.post_file(seg_id),
            docs: paths.docs_file(seg_id),
            syms: paths.syms_file(seg_id),
            refs: paths.refs_file(seg_id),
            live: paths.live_file(seg_id),
        };

        let valid = ValidSegment {
            fst: read_file(&files.fst),
            post: read_file(&files.post),
            docs: read_file(&files.docs),
            syms: read_file(&files.syms),
            refs: read_file(&files.refs),
            live: read_file(&files.live),
            meta: read_file(&meta_file),
        };

        Fixture {
            paths,
            seg_id,
            files,
            meta_file,
            valid,
        }
    })
}

fn restore(f: &Fixture) {
    let _ = std::fs::write(&f.files.fst, &f.valid.fst);
    let _ = std::fs::write(&f.files.post, &f.valid.post);
    let _ = std::fs::write(&f.files.docs, &f.valid.docs);
    let _ = std::fs::write(&f.files.syms, &f.valid.syms);
    let _ = std::fs::write(&f.files.refs, &f.valid.refs);
    let _ = std::fs::write(&f.files.live, &f.valid.live);
    let _ = std::fs::write(&f.meta_file, &f.valid.meta);
}

fn exercise_segment(seg: &Segment) {
    for needle in NEEDLES {
        let q = TrigramQuery::from_literal(needle);
        let _ = seg.candidates(&q);
    }
    let n = seg.docs.len().min(8);
    for doc_id in 0..n {
        let id = doc_id as u32;
        let _ = seg.doc(id);
        let _ = seg.doc_syms(id).count();
        let _ = seg.doc_refs(id).count();
        let _ = seg.is_live(id);
    }
    let _ = seg.all_live();
    let _ = seg.live_count();
    let _ = seg.calls_to("abcabc");
}

fn exercise_searcher(searcher: &Searcher) {
    for pat in ["abc", "fn ", "xyz_nomatch"] {
        let _ = searcher.search(&SearchQuery {
            pattern: pat.to_string(),
            ..Default::default()
        });
    }
    let _ = searcher.symbols(&SymbolQuery {
        name: "abcabc".to_string(),
        ..Default::default()
    });
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let f = fixture();

    // 0=fst, 1=post, 2=docs, 3=syms, 4=refs, 5=live, 6=meta
    let kind = data[0] as usize % 7;
    let payload = &data[1..];

    let target = match kind {
        0 => &f.files.fst,
        1 => &f.files.post,
        2 => &f.files.docs,
        3 => &f.files.syms,
        4 => &f.files.refs,
        5 => &f.files.live,
        _ => &f.meta_file,
    };
    if std::fs::write(target, payload).is_err() {
        return;
    }

    let _ = Meta::load(&f.meta_file);

    if kind != 6 {
        if let Ok(seg) = Segment::open(&f.paths, f.seg_id) {
            exercise_segment(&seg);
        }
        if let Ok(searcher) = Searcher::open(&f.paths) {
            exercise_searcher(&searcher);
        }
    }

    restore(f);
});
