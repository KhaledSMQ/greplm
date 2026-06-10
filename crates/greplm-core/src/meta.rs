//! Index manifest persisted at `.greplm/meta.json`.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// On-disk format version. Bump when the segment layout changes.
///
/// - v2 added the per-segment `seg-N.refs` reference/call-edge table.
/// - v3 switched the `docs`/`syms`/`refs` side tables from JSON to postcard
///   (compact binary) to cut on-disk size and cold-start parse time.
/// - v4 packs each posting list's cardinality into the FST value alongside its
///   offset, so query planning can intersect rarest-first without touching the
///   postings blob.
/// - v5 adds an xxh3 checksum footer to every segment file and the
///   `pending_tombstones` journal that makes incremental deletes atomic with
///   the manifest swap.
/// - v6 switches the `syms`/`refs` side tables to a columnar mmap format
///   (per-row offsets, doc CSR, packed name columns, persisted name FSTs)
///   so segment open is O(1) instead of a full decode.
pub const SCHEMA_VERSION: u32 = 6;

/// Tombstones that are published in the manifest but not yet applied to a
/// segment's on-disk live bitmap.
///
/// An incremental update publishes its new delta segment *and* the doc ids it
/// supersedes in a single atomic manifest write; the live bitmaps are only
/// mutated afterwards. Readers subtract any pending tombstones from the live
/// sets they load, and the next index operation applies and clears the journal
/// (idempotently), so a crash between the publish and the bitmap writes can
/// never surface deleted/stale documents nor lose the new ones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingTombstones {
    pub segment_id: u64,
    pub doc_ids: Vec<u32>,
}

/// Index-wide manifest describing the set of live segments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub schema_version: u32,
    /// IDs of segments that make up the current index.
    pub segments: Vec<u64>,
    /// Monotonic counter for allocating new segment IDs.
    pub next_segment_id: u64,
    /// Unix timestamp (seconds) of the last successful index operation.
    pub last_indexed: u64,
    /// Total live documents across all segments.
    pub doc_count: u64,
    /// Total symbols across all segments.
    pub symbol_count: u64,
    /// Git commit sha at the time of indexing (empty if not a repo). Lets
    /// callers detect that the working tree moved (e.g. a branch switch).
    #[serde(default)]
    pub indexed_git_head: String,
    /// Git branch name at the time of indexing (empty if not a repo).
    #[serde(default)]
    pub indexed_branch: String,
    /// Deletes published with the manifest but not yet applied to live
    /// bitmaps (see [`PendingTombstones`]). Normally empty; non-empty only in
    /// the window between an incremental publish and its bitmap writes (or
    /// after a crash inside that window, until the next index op recovers).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_tombstones: Vec<PendingTombstones>,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            segments: Vec::new(),
            next_segment_id: 0,
            last_indexed: 0,
            doc_count: 0,
            symbol_count: 0,
            indexed_git_head: String::new(),
            indexed_branch: String::new(),
            pending_tombstones: Vec::new(),
        }
    }
}

impl Meta {
    pub fn load(path: &Path) -> Result<Meta> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let meta: Meta = serde_json::from_slice(&bytes)?;
                if meta.schema_version != SCHEMA_VERSION {
                    return Err(Error::Corrupt(format!(
                        "index schema version {} != supported {}; run `greplm index` to rebuild",
                        meta.schema_version, SCHEMA_VERSION
                    )));
                }
                Ok(meta)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Meta::default()),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        crate::fsutil::write_atomic(path, &bytes)
    }

    /// Record the current git HEAD/branch for the indexed tree (best-effort).
    pub fn record_git_head(&mut self, root: &Path) {
        if let Some((sha, branch)) = crate::git::head(root) {
            self.indexed_git_head = sha;
            self.indexed_branch = branch;
        }
    }

    pub fn touch_now(&mut self) {
        self.last_indexed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
    }

    pub fn alloc_segment(&mut self) -> u64 {
        let id = self.next_segment_id;
        self.next_segment_id += 1;
        id
    }
}
