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

/// Walk the project root and return all candidate text files.
pub fn walk(paths: &Paths, config: &Config) -> Result<Vec<WalkEntry>> {
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

    // Parallel walk: worker threads send entries over a channel so traversal
    // overlaps with downstream processing and uses all cores.
    let (tx, rx) = channel::<WalkEntry>();
    let root = paths.root.clone();
    let max_size = config.max_file_size;
    builder.build_parallel().run(|| {
        let tx = tx.clone();
        let root = root.clone();
        Box::new(move |result| {
            let dent = match result {
                Ok(d) => d,
                Err(_) => return WalkState::Continue,
            };
            match dent.file_type() {
                Some(ft) if ft.is_file() => {}
                _ => return WalkState::Continue,
            }
            let metadata = match dent.metadata() {
                Ok(m) => m,
                Err(_) => return WalkState::Continue,
            };
            if metadata.len() > max_size || metadata.len() == 0 {
                return WalkState::Continue;
            }
            let path = dent.into_path();
            let rel = match path.strip_prefix(&root) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => path.to_string_lossy().to_string(),
            };
            let _ = tx.send(WalkEntry {
                path,
                rel,
                metadata,
            });
            WalkState::Continue
        })
    });
    drop(tx);
    Ok(rx.into_iter().collect())
}
