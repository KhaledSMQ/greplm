//! Fuzz the change-detection cache record decoder.
//!
//! The incremental-indexing cache stores per-file records as postcard-encoded
//! blobs in redb. Corrupt or truncated bytes must surface as errors from
//! `Cache::get` / `load_all`, never panics.
#![no_main]

use std::sync::atomic::{AtomicU64, Ordering};

use greplm_core::cache::{Cache, FileRecord};
use libfuzzer_sys::fuzz_target;
use redb::{Database, TableDefinition};

const FILES: TableDefinition<&str, &[u8]> = TableDefinition::new("files");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn sample_record() -> FileRecord {
    FileRecord {
        inode: 1,
        mtime_ns: 1,
        size: 10,
        hash: 42,
        segment_id: 0,
        doc_id: 0,
        symbols: 1,
    }
}

fuzz_target!(|data: &[u8]| {
    let _ = postcard::from_bytes::<FileRecord>(data);

    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut dir = std::env::temp_dir();
    dir.push(format!("greplm-fuzz-cache-{}-{n}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let db_path = dir.join("cache.redb");

    {
        let cache = match Cache::open(&db_path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = cache.apply(&[("good.rs".into(), sample_record())], &[]);
    }

    {
        let db = match Database::create(&db_path) {
            Ok(d) => d,
            Err(_) => {
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        let wtxn = match db.begin_write() {
            Ok(t) => t,
            Err(_) => {
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        {
            let mut table = match wtxn.open_table(FILES) {
                Ok(t) => t,
                Err(_) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    return;
                }
            };
            let _ = table.insert("bad.rs", data);
        }
        let _ = wtxn.commit();
    }

    if let Ok(cache) = Cache::open(&db_path) {
        let _ = cache.get("bad.rs");
        let _ = cache.load_all();
        let _ = cache.get("good.rs");
    }

    let _ = std::fs::remove_dir_all(&dir);
});
