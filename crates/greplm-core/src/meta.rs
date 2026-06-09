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
pub const SCHEMA_VERSION: u32 = 4;

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
