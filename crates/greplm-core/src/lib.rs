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
