//! Crash-injection durability tests.
//!
//! greplm publishes index files with a temp→fsync→rename discipline and only
//! swaps the manifest in once the new segment is durably written, so that a
//! crash (or power loss) mid-operation leaves the index either at its old state
//! or fully advanced — never half-written. This suite *proves* that property by
//! simulating a crash at **every** atomic-write boundary of an incremental
//! index and of a compaction, then asserting that a normal follow-up index
//! recovers to exactly what a from-scratch rebuild produces.
//!
//! The crash is injected via [`greplm_core::faults`], a test seam in the
//! atomic-write path: once armed at write index `n`, that write and every write
//! after it fail, faithfully modelling a process that dies and performs no
//! further writes. After the simulated crash we disarm, run the engine's
//! self-healing path, and compare a fingerprint of the recovered index against
//! ground truth.
//!
//! The global fault state forces these tests to run serially within this (own)
//! test binary; a mutex enforces that.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use greplm_core::faults;
use greplm_core::search::SearchQuery;
use greplm_core::Greplm;

/// Serializes access to the process-global fault-injection state.
static SERIAL: Mutex<()> = Mutex::new(());

/// Hard cap on how many write boundaries we probe, so a logic error can't loop
/// forever. Real operations here use well under this many atomic writes.
const MAX_WRITE_POINTS: u64 = 64;

/// Probe patterns whose exhaustive results fingerprint index contents.
const PROBES: &[&str] = &[
    "alpha_marker",
    "beta_marker",
    "gamma_marker",
    "delta_marker",
    "shared_token",
    "updated_body",
];

fn unique_tmp(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "greplm-crash-{tag}-{}-{nanos}-{n}",
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

/// A content fingerprint: index-wide counts plus exhaustive match locations for
/// each probe. Two indexes over the same working tree must fingerprint equal.
type Fingerprint = (u64, u64, Vec<(String, Vec<(String, u32, u32)>)>);

fn fingerprint(g: &Greplm) -> Fingerprint {
    let status = g.status().unwrap();
    let searcher = g.searcher().unwrap();
    let probes = PROBES
        .iter()
        .map(|pat| {
            let hits = searcher
                .search(&SearchQuery {
                    pattern: (*pat).to_string(),
                    exhaustive: true,
                    ..Default::default()
                })
                .unwrap();
            let keys = hits
                .iter()
                .map(|h| (h.path.clone(), h.line, h.column))
                .collect::<Vec<_>>();
            ((*pat).to_string(), keys)
        })
        .collect();
    (status.doc_count, status.symbol_count, probes)
}

/// Lay down the initial, successfully-indexed tree.
fn setup_baseline(root: &Path) -> Greplm {
    write(
        root,
        "src/a.rs",
        "fn alpha_marker() {\n    shared_token();\n}\n",
    );
    write(
        root,
        "src/b.rs",
        "fn beta_marker() {\n    shared_token();\n}\n",
    );
    write(root, "src/c.rs", "fn gamma_marker() {}\n");
    let g = Greplm::open(root).unwrap();
    g.index(true).unwrap();
    g
}

/// Apply a representative mix of changes (modify + add + delete) that the next
/// incremental index must durably absorb.
fn apply_changes(root: &Path) {
    // Modify a.rs (content + size change so it's detected and re-indexed).
    write(
        root,
        "src/a.rs",
        "fn alpha_marker() {\n    let updated_body = shared_token();\n    drop(updated_body);\n}\n",
    );
    // Add a new file.
    write(
        root,
        "src/d.rs",
        "fn delta_marker() {\n    shared_token();\n}\n",
    );
    // Delete an existing file.
    std::fs::remove_file(root.join("src/c.rs")).unwrap();
}

/// Verify that, after whatever partial state a crash left behind, the engine's
/// self-healing index recovers to exactly a from-scratch rebuild — and only
/// then return. Computes the recovered fingerprint *before* the ground-truth
/// rebuild (which mutates the same `.greplm`).
fn assert_recovers_to_ground_truth(g: &Greplm, scenario: &str, n: u64) {
    // Self-heal: a normal incremental must not error and must leave a queryable
    // index, no matter where the crash struck.
    g.index(false)
        .unwrap_or_else(|e| panic!("{scenario}: recovery index failed after crash @#{n}: {e:?}"));
    let recovered = fingerprint(g);

    // Ground truth: a full rebuild of the identical working tree.
    g.index(true).unwrap();
    let truth = fingerprint(g);

    assert_eq!(
        recovered, truth,
        "{scenario}: recovered index differs from a full rebuild after crash @#{n}"
    );
}

#[test]
fn incremental_index_survives_crash_at_every_write() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let mut n = 0u64;
    loop {
        assert!(n < MAX_WRITE_POINTS, "too many write points; logic error?");
        let root = unique_tmp("incr");
        let g = setup_baseline(&root);
        apply_changes(&root);

        faults::arm(n);
        let crashed = g.index(false).is_err();
        faults::disarm();

        // The first write boundary must actually be reachable (sanity that the
        // injection fires at all).
        if n == 0 {
            assert!(crashed, "arming at write #0 should crash the incremental");
        }

        assert_recovers_to_ground_truth(&g, "incremental", n);
        std::fs::remove_dir_all(&root).ok();

        // Once arming past the last write no longer crashes, we've covered every
        // boundary.
        if !crashed {
            break;
        }
        n += 1;
    }

    // We should have probed a meaningful number of distinct write points.
    assert!(
        n >= 3,
        "expected several incremental write boundaries, got {n}"
    );
}

#[test]
fn compaction_survives_crash_at_every_write() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let mut n = 0u64;
    loop {
        assert!(n < MAX_WRITE_POINTS, "too many write points; logic error?");
        let root = unique_tmp("compact");

        // Build an index with several segments so compaction has real work: a
        // baseline plus a few incremental deltas.
        let g = setup_baseline(&root);
        for i in 0..3 {
            write(
                &root,
                &format!("src/gen{i}.rs"),
                &format!("fn gen{i}_fn() {{ shared_token(); }}\n"),
            );
            g.index(false).unwrap();
        }

        faults::arm(n);
        let crashed = g.compact().is_err();
        faults::disarm();

        if n == 0 {
            assert!(crashed, "arming at write #0 should crash compaction");
        }

        assert_recovers_to_ground_truth(&g, "compaction", n);
        std::fs::remove_dir_all(&root).ok();

        if !crashed {
            break;
        }
        n += 1;
    }

    assert!(
        n >= 3,
        "expected several compaction write boundaries, got {n}"
    );
}
