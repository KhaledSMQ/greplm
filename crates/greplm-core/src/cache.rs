//! Change-detection cache backed by redb.
//!
//! For each indexed file we remember `(inode, mtime, size, content_hash)` plus
//! where the current version lives `(segment_id, doc_id)`. On a re-index or a
//! filesystem event we do a cheap `mtime`+`size` pre-check before hashing, and
//! only re-index when the fast hash actually changed. This rejects spurious
//! touches and lets us tombstone the stale doc.
//!
//! The cache is purely an optimization: it can always be rebuilt by re-indexing.
//! That property drives two design choices below — a stored schema version that
//! wipes the cache on any `FileRecord` layout change (degrading to a re-index
//! instead of a hard deserialize error), and relaxed write durability on the hot
//! `apply` path (a crash just costs a re-index).

use redb::{Database, Durability, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::Result;

const FILES: TableDefinition<&str, &[u8]> = TableDefinition::new("files");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

/// Bump whenever `FileRecord`'s layout or the serialization format changes.
/// A mismatch at open time wipes the cache so stale records can't be misread.
const SCHEMA_VERSION: u64 = 2;
const SCHEMA_KEY: &str = "schema_version";

/// Per-file record used for incremental indexing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    /// Filesystem inode. Always `0` on non-unix platforms, so any inode-based
    /// rename/move detection downstream is effectively disabled there.
    pub inode: u64,
    /// Modification time in nanoseconds since the unix epoch. `i64` holds current
    /// epochs comfortably (overflows ~year 2262).
    pub mtime_ns: i64,
    pub size: u64,
    pub hash: u64,
    pub segment_id: u64,
    pub doc_id: u32,
    /// Number of symbols this document contributed. Lets the indexer maintain
    /// the index-wide symbol count incrementally instead of re-parsing every
    /// segment on each update.
    pub symbols: u32,
}

/// Handle to the on-disk change-detection cache.
pub struct Cache {
    db: Database,
}

impl Cache {
    pub fn open(path: &Path) -> Result<Cache> {
        let db = Database::create(path)?;
        // Ensure the tables exist so read transactions don't fail on a fresh db.
        let wtxn = db.begin_write()?;
        {
            let _ = wtxn.open_table(FILES)?;
            let _ = wtxn.open_table(META)?;
        }
        wtxn.commit()?;
        let cache = Cache { db };
        cache.ensure_schema()?;
        Ok(cache)
    }

    /// Wipe the cache if the stored schema version doesn't match the current one
    /// (or is absent, i.e. first run). The data is rebuildable, so this is a
    /// cache miss rather than an error.
    fn ensure_schema(&self) -> Result<()> {
        let stored = {
            let rtxn = self.db.begin_read()?;
            let table = rtxn.open_table(META)?;
            table.get(SCHEMA_KEY)?.map(|v| v.value())
        };
        if stored != Some(SCHEMA_VERSION) {
            self.clear()?;
            let wtxn = self.db.begin_write()?;
            {
                let mut table = wtxn.open_table(META)?;
                table.insert(SCHEMA_KEY, SCHEMA_VERSION)?;
            }
            wtxn.commit()?;
        }
        Ok(())
    }

    pub fn get(&self, path: &str) -> Result<Option<FileRecord>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(FILES)?;
        match table.get(path)? {
            Some(v) => Ok(Some(postcard::from_bytes(v.value())?)),
            None => Ok(None),
        }
    }

    /// Load all records into a map keyed by path.
    pub fn load_all(&self) -> Result<std::collections::HashMap<String, FileRecord>> {
        let rtxn = self.db.begin_read()?;
        let table = rtxn.open_table(FILES)?;
        let mut out = std::collections::HashMap::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            let rec: FileRecord = postcard::from_bytes(v.value())?;
            out.insert(k.value().to_string(), rec);
        }
        Ok(out)
    }

    /// Apply a batch of inserts and deletes in a single transaction.
    pub fn apply(&self, upserts: &[(String, FileRecord)], deletes: &[String]) -> Result<()> {
        // Serialize outside the write lock to keep the single-writer hold short.
        let encoded: Vec<(&str, Vec<u8>)> = upserts
            .iter()
            .map(|(path, rec)| Ok((path.as_str(), postcard::to_allocvec(rec)?)))
            .collect::<Result<_>>()?;

        let mut wtxn = self.db.begin_write()?;
        // The cache is rebuildable, so skip fsync on this hot (watch-event) path;
        // a crash just costs a re-index, which is the fallback we already support.
        wtxn.set_durability(Durability::None);
        {
            let mut table = wtxn.open_table(FILES)?;
            for (path, bytes) in &encoded {
                table.insert(*path, bytes.as_slice())?;
            }
            for path in deletes {
                table.remove(path.as_str())?;
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    /// Replace the entire cache contents with `records` in a single transaction.
    /// Used after compaction, when every live document gets new segment/doc ids.
    pub fn replace_all(&self, records: &[(String, FileRecord)]) -> Result<()> {
        let encoded: Vec<(&str, Vec<u8>)> = records
            .iter()
            .map(|(path, rec)| Ok((path.as_str(), postcard::to_allocvec(rec)?)))
            .collect::<Result<_>>()?;

        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(FILES)?;
            // Drop every existing entry; `retain` propagates iteration errors
            // instead of silently skipping them, so stale data can't survive.
            table.retain(|_, _| false)?;
            for (path, bytes) in &encoded {
                table.insert(*path, bytes.as_slice())?;
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        self.replace_all(&[])
    }
}

/// Compute the fast content hash used for change detection.
pub fn fast_hash(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}

/// Extract `(inode, mtime_ns, size)` from filesystem metadata.
#[cfg(unix)]
pub fn stat_key(meta: &std::fs::Metadata) -> (u64, i64, u64) {
    use std::os::unix::fs::MetadataExt;
    let mtime_ns = meta.mtime() * 1_000_000_000 + meta.mtime_nsec();
    (meta.ino(), mtime_ns, meta.len())
}

#[cfg(not(unix))]
pub fn stat_key(meta: &std::fs::Metadata) -> (u64, i64, u64) {
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    (0, mtime_ns, meta.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Unique temp file path for an isolated redb instance, cleaned up on drop.
    struct TempDb(PathBuf);

    impl TempDb {
        fn new(tag: &str) -> TempDb {
            let mut p = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            p.push(format!("greplm-cache-{tag}-{nanos}.redb"));
            TempDb(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn rec(seed: u64) -> FileRecord {
        FileRecord {
            inode: seed,
            mtime_ns: seed as i64 * 1_000_000_000 + 7,
            size: seed * 13,
            hash: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15),
            segment_id: seed + 100,
            doc_id: seed as u32 + 5,
            symbols: seed as u32 * 2,
        }
    }

    fn assert_same(a: &FileRecord, b: &FileRecord) {
        assert_eq!(a.inode, b.inode);
        assert_eq!(a.mtime_ns, b.mtime_ns);
        assert_eq!(a.size, b.size);
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.segment_id, b.segment_id);
        assert_eq!(a.doc_id, b.doc_id);
        assert_eq!(a.symbols, b.symbols);
    }

    #[test]
    fn fresh_db_is_empty_and_get_misses() {
        let tmp = TempDb::new("fresh");
        let cache = Cache::open(tmp.path()).unwrap();
        assert!(cache.get("anything").unwrap().is_none());
        assert!(cache.load_all().unwrap().is_empty());
    }

    #[test]
    fn apply_roundtrips_all_fields() {
        let tmp = TempDb::new("roundtrip");
        let cache = Cache::open(tmp.path()).unwrap();
        let r = rec(42);
        cache.apply(&[("src/a.rs".into(), r.clone())], &[]).unwrap();

        let got = cache.get("src/a.rs").unwrap().expect("record present");
        assert_same(&got, &r);

        let all = cache.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_same(&all["src/a.rs"], &r);
    }

    #[test]
    fn apply_upserts_and_deletes() {
        let tmp = TempDb::new("upsert");
        let cache = Cache::open(tmp.path()).unwrap();
        cache
            .apply(
                &[
                    ("a".into(), rec(1)),
                    ("b".into(), rec(2)),
                    ("c".into(), rec(3)),
                ],
                &[],
            )
            .unwrap();

        // Overwrite "a" and delete "b" in one batch.
        cache
            .apply(&[("a".into(), rec(99))], &["b".to_string()])
            .unwrap();

        let all = cache.load_all().unwrap();
        assert_eq!(all.len(), 2);
        assert_same(&all["a"], &rec(99));
        assert!(!all.contains_key("b"));
        assert_same(&all["c"], &rec(3));
    }

    #[test]
    fn replace_all_wipes_then_inserts() {
        let tmp = TempDb::new("replace");
        let cache = Cache::open(tmp.path()).unwrap();
        cache
            .apply(&[("old1".into(), rec(1)), ("old2".into(), rec(2))], &[])
            .unwrap();

        cache
            .replace_all(&[("new1".into(), rec(10)), ("new2".into(), rec(20))])
            .unwrap();

        let all = cache.load_all().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.contains_key("new1") && all.contains_key("new2"));
        assert!(!all.contains_key("old1") && !all.contains_key("old2"));
        assert_same(&all["new1"], &rec(10));
    }

    #[test]
    fn clear_empties_everything() {
        let tmp = TempDb::new("clear");
        let cache = Cache::open(tmp.path()).unwrap();
        cache
            .apply(&[("a".into(), rec(1)), ("b".into(), rec(2))], &[])
            .unwrap();
        cache.clear().unwrap();
        assert!(cache.load_all().unwrap().is_empty());

        // Clear on an already-empty cache is a no-op, not an error.
        cache.clear().unwrap();
        assert!(cache.load_all().unwrap().is_empty());
    }

    #[test]
    fn data_survives_reopen_with_matching_schema() {
        let tmp = TempDb::new("persist");
        {
            let cache = Cache::open(tmp.path()).unwrap();
            cache.apply(&[("keep.rs".into(), rec(7))], &[]).unwrap();
        }
        // Reopen: matching schema version must NOT wipe existing data.
        let cache = Cache::open(tmp.path()).unwrap();
        let got = cache.get("keep.rs").unwrap().expect("survives reopen");
        assert_same(&got, &rec(7));
    }

    #[test]
    fn schema_version_mismatch_wipes_cache() {
        let tmp = TempDb::new("schema");
        {
            let cache = Cache::open(tmp.path()).unwrap();
            cache.apply(&[("stale.rs".into(), rec(3))], &[]).unwrap();
            assert_eq!(cache.load_all().unwrap().len(), 1);
        }

        // Simulate a layout/format change by storing a different schema version,
        // mimicking an on-disk cache written by an incompatible build.
        {
            let db = Database::create(tmp.path()).unwrap();
            let wtxn = db.begin_write().unwrap();
            {
                let mut t = wtxn.open_table(META).unwrap();
                t.insert(SCHEMA_KEY, SCHEMA_VERSION + 1).unwrap();
            }
            wtxn.commit().unwrap();
        }

        // Opening detects the mismatch and degrades to a clean (empty) cache
        // rather than surfacing stale/undecodable records.
        let cache = Cache::open(tmp.path()).unwrap();
        assert!(
            cache.load_all().unwrap().is_empty(),
            "schema mismatch should wipe stale records"
        );

        // And the cache is usable again afterwards.
        cache.apply(&[("fresh.rs".into(), rec(8))], &[]).unwrap();
        assert_same(&cache.get("fresh.rs").unwrap().unwrap(), &rec(8));
    }

    #[test]
    fn apply_deletes_only() {
        let tmp = TempDb::new("delete-only");
        let cache = Cache::open(tmp.path()).unwrap();
        cache
            .apply(&[("a".into(), rec(1)), ("b".into(), rec(2))], &[])
            .unwrap();

        cache
            .apply(&[], &["a".to_string(), "b".to_string()])
            .unwrap();

        assert!(cache.load_all().unwrap().is_empty());
        assert!(cache.get("a").unwrap().is_none());
    }

    #[test]
    fn apply_empty_batch_is_noop() {
        let tmp = TempDb::new("empty-batch");
        let cache = Cache::open(tmp.path()).unwrap();
        cache.apply(&[("a".into(), rec(1))], &[]).unwrap();

        cache.apply(&[], &[]).unwrap();

        let all = cache.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_same(&all["a"], &rec(1));
    }

    #[test]
    fn replace_all_on_empty_cache_is_noop() {
        let tmp = TempDb::new("replace-empty");
        let cache = Cache::open(tmp.path()).unwrap();
        cache.replace_all(&[]).unwrap();
        assert!(cache.load_all().unwrap().is_empty());
    }

    #[test]
    fn missing_schema_key_wipes_legacy_records() {
        let tmp = TempDb::new("legacy-schema");
        // Simulate a pre-versioning cache: file records present, no schema key.
        {
            let db = Database::create(tmp.path()).unwrap();
            let wtxn = db.begin_write().unwrap();
            {
                let mut files = wtxn.open_table(FILES).unwrap();
                let bytes = postcard::to_allocvec(&rec(3)).unwrap();
                files.insert("legacy.rs", bytes.as_slice()).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let cache = Cache::open(tmp.path()).unwrap();
        assert!(
            cache.load_all().unwrap().is_empty(),
            "absent schema key should wipe legacy records"
        );

        cache.apply(&[("fresh.rs".into(), rec(8))], &[]).unwrap();
        assert_same(&cache.get("fresh.rs").unwrap().unwrap(), &rec(8));
    }

    #[test]
    fn corrupt_record_bytes_surface_as_errors() {
        let tmp = TempDb::new("corrupt");
        let valid = rec(1);
        // Truncate a valid encoding: arbitrary bytes can still decode as nonsense
        // values without error, but a truncated message must fail.
        let mut truncated = postcard::to_allocvec(&valid).unwrap();
        truncated.truncate(truncated.len().saturating_sub(1));

        {
            let cache = Cache::open(tmp.path()).unwrap();
            cache
                .apply(&[("good.rs".into(), valid.clone())], &[])
                .unwrap();
        }
        {
            let db = Database::create(tmp.path()).unwrap();
            let wtxn = db.begin_write().unwrap();
            {
                let mut table = wtxn.open_table(FILES).unwrap();
                table.insert("bad.rs", truncated.as_slice()).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let cache = Cache::open(tmp.path()).unwrap();
        assert!(
            cache.get("bad.rs").is_err(),
            "truncated record should fail decode"
        );
        assert!(
            cache.load_all().is_err(),
            "load_all should fail on undecodable records"
        );
        // Unaffected keys remain readable.
        assert_same(&cache.get("good.rs").unwrap().unwrap(), &valid);
    }

    #[test]
    fn stat_key_matches_file_metadata() {
        let mut path = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!("greplm-stat-{nanos}.txt"));
        let contents = b"greplm stat_key probe";
        std::fs::write(&path, contents).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let (inode, mtime_ns, size) = stat_key(&meta);

        assert_eq!(size, contents.len() as u64);
        assert!(
            mtime_ns > 0,
            "mtime_ns should reflect file modification time"
        );
        #[cfg(unix)]
        assert!(inode > 0, "unix inode should be non-zero");
        #[cfg(not(unix))]
        assert_eq!(inode, 0, "non-unix platforms disable inode detection");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fast_hash_is_stable_and_distinguishes() {
        assert_eq!(fast_hash(b"hello world"), fast_hash(b"hello world"));
        assert_ne!(fast_hash(b"hello world"), fast_hash(b"hello worle"));

        assert_eq!(fast_hash(b""), fast_hash(b""));

        let large = vec![0xABu8; 1_000_000];
        assert_eq!(fast_hash(&large), fast_hash(&large));
        assert_ne!(fast_hash(&large), fast_hash(b"small"));
    }
}
