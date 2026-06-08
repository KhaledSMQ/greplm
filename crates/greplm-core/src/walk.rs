//! Filesystem traversal.
//!
//! Uses ripgrep's `ignore` crate, which honors `.gitignore`/`.ignore`, prunes
//! ignored directories early, and uses `d_type` from `getdents64` to avoid an
//! extra `stat` per entry. We walk sequentially (the walk is rarely the
//! bottleneck) and feed the resulting file list to a parallel read/parse stage.

use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};
use std::fs::Metadata;
use std::path::PathBuf;
use std::sync::mpsc::channel;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::paths::Paths;

/// A file discovered during the walk.
#[derive(Debug, Clone)]
pub struct WalkEntry {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Path relative to the project root, using `/` separators.
    pub rel: String,
    pub metadata: Metadata,
}

/// Why a file `grep` would have searched was left out of the index. Used to make
/// exclusions visible instead of silently dropping files. Note that files pruned
/// by `.gitignore`/hidden rules are removed inside the `ignore` crate before the
/// closure runs and are therefore *not* itemized here (by design — that pruning
/// is the configured intent, not a surprise).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Larger than `max_file_size`.
    TooLarge,
    /// Zero-byte file (and `index_empty` is off).
    Empty,
    /// Contains NUL bytes (and `index_binary` is off).
    Binary,
    /// The file could not be read.
    ReadError,
    /// The directory walk reported an error for this entry.
    WalkError,
    /// `stat`/metadata lookup failed.
    StatError,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::TooLarge => "too_large",
            SkipReason::Empty => "empty",
            SkipReason::Binary => "binary",
            SkipReason::ReadError => "read_error",
            SkipReason::WalkError => "walk_error",
            SkipReason::StatError => "stat_error",
        }
    }
}

/// A file that was skipped, with the reason.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Skipped {
    /// Path relative to the project root (best-effort; falls back to the
    /// absolute path when it can't be made relative).
    pub rel: String,
    pub reason: SkipReason,
}

/// The product of a walk: the indexable entries plus the files that were
/// skipped during traversal (size/empty/error). Binary and read-error skips are
/// detected later, in the indexer's read stage.
#[derive(Debug, Default)]
pub struct WalkResult {
    pub entries: Vec<WalkEntry>,
    pub skipped: Vec<Skipped>,
}

/// Walk the project root and return all candidate text files plus skip records.
pub fn walk(paths: &Paths, config: &Config) -> Result<WalkResult> {
    let mut overrides = OverrideBuilder::new(&paths.root);
    for pat in &config.include {
        overrides
            .add(pat)
            .map_err(|e| Error::other(format!("bad include glob {pat:?}: {e}")))?;
    }
    for pat in &config.exclude {
        overrides
            .add(&format!("!{pat}"))
            .map_err(|e| Error::other(format!("bad exclude glob {pat:?}: {e}")))?;
    }
    let overrides = overrides
        .build()
        .map_err(|e| Error::other(format!("invalid overrides: {e}")))?;

    let mut builder = WalkBuilder::new(&paths.root);
    builder
        .hidden(!config.index_hidden)
        .git_ignore(config.respect_gitignore)
        .git_global(config.respect_gitignore)
        .git_exclude(config.respect_gitignore)
        .ignore(config.respect_gitignore)
        .parents(config.respect_gitignore)
        .overrides(overrides)
        .follow_links(false);

    // Parallel walk: worker threads send entries (and skip records) over
    // channels so traversal overlaps with downstream processing and uses all
    // cores.
    let (tx, rx) = channel::<WalkEntry>();
    let (stx, srx) = channel::<Skipped>();
    let root = paths.root.clone();
    // `0` means "no size cap" (grep parity).
    let max_size = config.max_file_size;
    let index_empty = config.index_empty;
    builder.build_parallel().run(|| {
        let tx = tx.clone();
        let stx = stx.clone();
        let root = root.clone();
        let rel_of = move |p: &std::path::Path| match p.strip_prefix(&root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => p.to_string_lossy().to_string(),
        };
        Box::new(move |result| {
            let dent = match result {
                Ok(d) => d,
                Err(e) => {
                    // `ignore::Error` doesn't reliably expose a path; keep the
                    // message so the skip is still attributable.
                    let _ = stx.send(Skipped {
                        rel: format!("<walk error: {e}>"),
                        reason: SkipReason::WalkError,
                    });
                    return WalkState::Continue;
                }
            };
            match dent.file_type() {
                Some(ft) if ft.is_file() => {}
                _ => return WalkState::Continue,
            }
            let metadata = match dent.metadata() {
                Ok(m) => m,
                Err(_) => {
                    let _ = stx.send(Skipped {
                        rel: rel_of(dent.path()),
                        reason: SkipReason::StatError,
                    });
                    return WalkState::Continue;
                }
            };
            let path = dent.into_path();
            let rel = rel_of(&path);
            let len = metadata.len();
            if len == 0 {
                if !index_empty {
                    let _ = stx.send(Skipped {
                        rel,
                        reason: SkipReason::Empty,
                    });
                }
                return WalkState::Continue;
            }
            if max_size != 0 && len > max_size {
                let _ = stx.send(Skipped {
                    rel,
                    reason: SkipReason::TooLarge,
                });
                return WalkState::Continue;
            }
            let _ = tx.send(WalkEntry {
                path,
                rel,
                metadata,
            });
            WalkState::Continue
        })
    });
    drop(tx);
    drop(stx);
    Ok(WalkResult {
        entries: rx.into_iter().collect(),
        skipped: srx.into_iter().collect(),
    })
}
