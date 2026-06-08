//! Fuzz the on-disk postings decoder against a corrupt index.
//!
//! A `.greplm` index is untrusted in practice: it can be truncated by a crash,
//! corrupted on disk, or shipped between machines. Opening and querying a
//! segment must degrade to a clean `Err` (which the engine's self-healing path
//! turns into a rebuild), never a panic.
//!
//! Strategy: build one *valid* segment once, then on each input overwrite only
//! the postings blob (`seg-*.post`) with fuzzer bytes while leaving the FST
//! intact. The FST still maps known trigrams to byte offsets, so querying a
//! trigram that exists in the corpus forces the decoder to interpret the
//! corrupt blob at those offsets — exactly the adversarial path.
#![no_main]

use std::path::PathBuf;
use std::sync::OnceLock;

use greplm_core::meta::Meta;
use greplm_core::paths::Paths;
use greplm_core::segment::Segment;
use greplm_core::trigram::TrigramQuery;
use greplm_core::Greplm;
use libfuzzer_sys::fuzz_target;

struct Fixture {
    paths: Paths,
    seg_id: u64,
    post_file: PathBuf,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let mut dir = std::env::temp_dir();
        dir.push(format!("greplm-fuzz-seg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create fuzz dir");

        // Content chosen so the FST contains common, easily-queried trigrams
        // ("abc", "fn ", "let", ...).
        std::fs::write(
            dir.join("sample.rs"),
            b"fn abcabc() {\n    let abcdef = 123;\n    call_abc(abcdef);\n}\n",
        )
        .expect("write sample");

        let g = Greplm::open(&dir).expect("open project");
        g.index(true).expect("build index");

        let paths = Paths::new(&dir);
        let meta = Meta::load(&paths.meta_file()).expect("load meta");
        let seg_id = *meta.segments.first().expect("at least one segment");
        let post_file = paths.post_file(seg_id);

        Fixture {
            paths,
            seg_id,
            post_file,
        }
    })
}

fuzz_target!(|data: &[u8]| {
    let f = fixture();

    // Replace the postings blob with fuzzer-controlled bytes. The previous
    // segment's mmap is dropped at the end of each iteration, so this write is
    // safe.
    if std::fs::write(&f.post_file, data).is_err() {
        return;
    }

    if let Ok(seg) = Segment::open(&f.paths, f.seg_id) {
        for needle in [&b"abc"[..], b"fn ", b"let", b"call"] {
            let q = TrigramQuery::from_literal(needle);
            let _ = seg.candidates(&q);
        }
    }
});
