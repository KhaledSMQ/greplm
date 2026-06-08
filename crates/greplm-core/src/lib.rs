//! greplm-core: an extreme-performance, trigram-based code indexer for LLM agents.
//!
//! The index lives in a `.greplm/` directory at the project root and consists of
//! immutable, mmap-backed segments (trigram FST + roaring posting lists + doc and
//! symbol tables). Search filters candidate documents by trigram intersection,
//! then verifies matches with the real literal/regex matcher.

mod error;

pub mod cache;
pub mod client;
pub mod config;
pub mod context;
pub mod daemon;
pub(crate) mod fsutil;
pub mod git;
pub mod indexer;
pub mod io_backend;
pub mod lang;
pub mod meta;
pub mod paths;
pub mod proto;
pub mod resolve;
pub mod savings;
pub mod search;
pub mod segment;
#[cfg(feature = "semantic")]
pub mod semantic;
pub mod structural;
pub mod symbol;
pub mod trigram;
pub mod walk;
pub mod watch;

pub use error::{Error, Result};

/// Test-only crash/fault-injection seam for the atomic-write path (see
/// [`fsutil::faults`]). Hidden from public docs; used by greplm's own
/// durability tests to simulate crashes mid-index/compaction.
#[doc(hidden)]
pub use fsutil::faults;

use std::path::{Path, PathBuf};

use config::Config;
use indexer::{IndexStats, Indexer};
use io_backend::IoBackend;
use meta::Meta;
use paths::Paths;
use search::Searcher;

/// Status snapshot for `greplm status`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Status {
    pub root: PathBuf,
    pub indexed: bool,
    pub segments: usize,
    pub doc_count: u64,
    pub symbol_count: u64,
    pub last_indexed: u64,
    pub backend: String,
}

/// A handle to a greplm project (a directory and its `.greplm` index).
pub struct Greplm {
    paths: Paths,
    config: Config,
    backend: Box<dyn IoBackend>,
}

impl Greplm {
    /// Open (and lazily initialize) the index for `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Greplm> {
        let paths = Paths::new(root);
        let config = Config::load(&paths.config_file())?;
        Ok(Greplm {
            paths,
            config,
            backend: io_backend::default_backend(),
        })
    }

    /// Find the nearest ancestor of `start` containing a `.greplm` directory,
    /// falling back to `start` itself if none is found.
    pub fn discover(start: impl AsRef<Path>) -> Result<Greplm> {
        let start = start.as_ref();
        let mut cur = Some(start);
        while let Some(dir) = cur {
            if dir.join(paths::DIR_NAME).is_dir() {
                return Greplm::open(dir);
            }
            cur = dir.parent();
        }
        Greplm::open(start)
    }

    pub fn root(&self) -> &Path {
        &self.paths.root
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Ensure the `.greplm` directory exists with a default config and gitignore.
    pub fn ensure_initialized(&self) -> Result<()> {
        std::fs::create_dir_all(self.paths.segments_dir())
            .map_err(|e| Error::io(self.paths.segments_dir(), e))?;
        let cfg = self.paths.config_file();
        if !cfg.exists() {
            self.config.save(&cfg)?;
        }
        let gi = self.paths.gitignore_file();
        if !gi.exists() {
            std::fs::write(&gi, "*\n").map_err(|e| Error::io(&gi, e))?;
        }
        Ok(())
    }

    /// Build or refresh the index.
    pub fn index(&self, force: bool) -> Result<IndexStats> {
        self.ensure_initialized()?;
        let indexer = Indexer::new(&self.paths, &self.config, self.backend.as_ref());
        if force {
            indexer.index_full()
        } else {
            indexer.index_incremental()
        }
    }

    /// Ensure a usable, current-schema index exists, building it if absent,
    /// empty, or left unreadable by an on-disk format change. Returns `true` if
    /// a (re)build happened. This is the self-healing entry point for query
    /// paths: a fresh checkout or a post-upgrade stale index transparently
    /// builds instead of erroring with "run `greplm index` first".
    ///
    /// Cheap when a good index already exists (one manifest read). The actual
    /// rebuild-on-corrupt logic lives in [`Indexer::index_incremental`], which
    /// falls back to a full rebuild on an unreadable/outdated manifest.
    pub fn ensure_indexed(&self) -> Result<bool> {
        match self.status() {
            // A populated, current-schema index — nothing to do.
            Ok(s) if s.indexed => return Ok(false),
            // Initialized but empty, or readable-but-empty manifest: build.
            Ok(_) => {}
            // Unreadable/outdated manifest (e.g. schema bump): index() rebuilds.
            Err(_) => {}
        }
        self.index(false)?;
        Ok(true)
    }

    /// Stat-only freshness probe: does any file on disk differ from what the
    /// index recorded (new, modified by size/mtime, or deleted)? No content
    /// hashing and no reads — just the same cheap pre-check the incremental
    /// indexer uses. The daemon calls this to guarantee read-after-write
    /// consistency: if dirty, it reindexes before answering.
    pub fn is_dirty(&self) -> Result<bool> {
        let cache = cache::Cache::open(&self.paths.cache_file())?;
        let existing = cache.load_all()?;
        let walked = walk::walk(&self.paths, &self.config)?;

        let mut seen = std::collections::HashSet::with_capacity(walked.entries.len());
        for e in &walked.entries {
            seen.insert(e.rel.clone());
            let (_, mtime_ns, size) = cache::stat_key(&e.metadata);
            match existing.get(&e.rel) {
                // Already indexed; a changed stat key means it may have changed.
                Some(rec) if rec.size == size && rec.mtime_ns == mtime_ns => {}
                Some(_) => return Ok(true),
                // Not in the index. It's only "dirty" if the indexer would
                // actually index it — files it intentionally skips (binary) are
                // never cached, so counting them would make any project with a
                // binary file look perpetually stale.
                None => {
                    if self.would_index(&e.path) {
                        return Ok(true);
                    }
                }
            }
        }
        // A previously indexed file that disappeared (or became un-indexable,
        // e.g. now too large/binary and dropped from the walk) is also dirty.
        for path in existing.keys() {
            if !seen.contains(path) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Would the indexer index this not-yet-cached file, or skip it the way its
    /// read stage does (binary content, unreadable)? Mirrors `indexer::process`
    /// so [`is_dirty`](Self::is_dirty) doesn't flag intentionally-skipped files.
    fn would_index(&self, path: &Path) -> bool {
        match std::fs::read(path) {
            Ok(data) => self.config.index_binary || memchr::memchr(0, &data).is_none(),
            Err(_) => false,
        }
    }

    /// Merge all segments into a single compact segment, dropping tombstoned
    /// documents. Falls back to a full rebuild if the merge cannot proceed.
    pub fn compact(&self) -> Result<IndexStats> {
        self.ensure_initialized()?;
        Indexer::new(&self.paths, &self.config, self.backend.as_ref()).compact()
    }

    /// Open a searcher over the current index.
    pub fn searcher(&self) -> Result<Searcher> {
        Searcher::open(&self.paths)
    }

    /// Content search that always returns results: it queries the index, and if
    /// the index is missing or errors, transparently falls back to an
    /// index-free walk+scan (grep parity). The fallback is logged at WARN.
    pub fn search_or_grep(&self, query: &search::SearchQuery) -> Result<Vec<search::SearchHit>> {
        match self.searcher().and_then(|s| s.search(query)) {
            Ok(hits) => Ok(hits),
            Err(e) => {
                tracing::warn!("index unavailable ({e}); falling back to grep walk");
                search::grep_walk(&self.paths, &self.config, query)
            }
        }
    }

    /// Report index status.
    pub fn status(&self) -> Result<Status> {
        let meta = if self.paths.meta_file().exists() {
            Meta::load(&self.paths.meta_file())?
        } else {
            Meta::default()
        };
        Ok(Status {
            root: self.paths.root.clone(),
            indexed: !meta.segments.is_empty(),
            segments: meta.segments.len(),
            doc_count: meta.doc_count,
            symbol_count: meta.symbol_count,
            last_indexed: meta.last_indexed,
            backend: self.backend.name().to_string(),
        })
    }

    /// Remove the entire `.greplm` directory.
    pub fn clean(&self) -> Result<()> {
        if self.paths.base.is_dir() {
            std::fs::remove_dir_all(&self.paths.base)
                .map_err(|e| Error::io(&self.paths.base, e))?;
        }
        Ok(())
    }

    /// Watch the project tree and re-index incrementally on changes.
    ///
    /// `on_change` is called after each successful incremental update. This call
    /// blocks until an error occurs or the watcher is dropped.
    pub fn watch<F: FnMut(&IndexStats)>(
        &self,
        debounce: std::time::Duration,
        on_change: F,
    ) -> Result<()> {
        watch::run(self, debounce, on_change)
    }

    /// Path to the daemon's Unix socket for this project.
    pub fn socket_path(&self) -> PathBuf {
        self.paths.base.join(proto::SOCKET_NAME)
    }

    /// Record a query's token savings (grep+read baseline vs. returned payload).
    /// Best-effort; never fails a query.
    pub fn record_savings(
        &self,
        kind: &str,
        files: &std::collections::BTreeSet<String>,
        returned_chars: u64,
        results: u64,
    ) {
        savings::record(&self.paths, kind, files, returned_chars, results);
    }

    /// Aggregate the recorded token-savings log.
    pub fn savings_report(&self) -> savings::SavingsReport {
        savings::report(&self.paths)
    }

    pub(crate) fn paths(&self) -> &Paths {
        &self.paths
    }
}
