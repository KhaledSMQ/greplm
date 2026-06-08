//! Live incremental indexing driven by filesystem events.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::{Override, OverrideBuilder};

use notify::{RecursiveMode, Watcher};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::indexer::IndexStats;
use crate::paths::Paths;
use crate::Greplm;

/// Watch the project tree and re-index on changes, debounced by `debounce`.
///
/// The loop is resilient: a failed incremental index is logged and the watcher
/// keeps running, and watcher-level errors are logged rather than silently
/// dropped. It returns only when the watcher is dropped/disconnected.
pub fn run<F: FnMut(&IndexStats)>(
    greplm: &Greplm,
    debounce: Duration,
    mut on_change: F,
) -> Result<()> {
    let filter = Filter::new(greplm.paths(), greplm.config())?;

    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        // Forward events; ignore send errors after the receiver is gone.
        let _ = tx.send(res);
    })
    .map_err(|e| Error::other(format!("watcher init: {e}")))?;

    watcher
        .watch(greplm.root(), RecursiveMode::Recursive)
        .map_err(|e| Error::other(format!("watch: {e}")))?;

    loop {
        // Block for the first relevant event.
        match rx.recv() {
            Ok(Ok(ev)) => {
                if !filter.is_relevant(&ev) {
                    continue;
                }
            }
            Ok(Err(e)) => {
                // e.g. inotify watch-limit exhaustion on Linux. Surface it
                // instead of silently missing future changes.
                tracing::warn!("watch event error: {e}");
                continue;
            }
            Err(_) => break, // watcher dropped
        }

        // Coalesce a burst of events within the debounce window. The deadline is
        // fixed so a steady stream of events can't extend the window forever.
        let deadline = Instant::now() + debounce;
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }

        // A single transient failure (a file removed mid-read, an I/O hiccup)
        // must not tear down live indexing. Log and keep watching; any events
        // that arrived during this pass remain queued and trigger the next one.
        match greplm.index(false) {
            Ok(stats) => on_change(&stats),
            Err(e) => tracing::warn!("incremental index failed: {e}"),
        }
    }
    Ok(())
}

/// Decides whether a filesystem event should trigger a re-index.
///
/// This mirrors the indexer's walk filtering (see `walk.rs`) so that churn in
/// ignored locations — `.greplm`, `.git`, `target`, `node_modules`, gitignored
/// or hidden paths — does not wake the indexer. The walk remains the source of
/// truth: anything that slips through here merely causes a no-op incremental
/// pass rather than an incorrect index.
struct Filter {
    root: PathBuf,
    base: PathBuf,
    overrides: Override,
    gitignore: Option<Gitignore>,
    index_hidden: bool,
}

impl Filter {
    fn new(paths: &Paths, config: &Config) -> Result<Self> {
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

        // Best-effort gitignore matching from the root `.gitignore`. Nested or
        // global gitignores aren't fully reconstructed here; the walk handles
        // those precisely and this only suppresses obvious trigger noise.
        let gitignore = if config.respect_gitignore {
            let mut builder = GitignoreBuilder::new(&paths.root);
            builder.add(paths.root.join(".gitignore"));
            builder.build().ok().filter(|gi| gi.num_ignores() > 0)
        } else {
            None
        };

        Ok(Self {
            root: paths.root.clone(),
            base: paths.base.clone(),
            overrides,
            gitignore,
            index_hidden: config.index_hidden,
        })
    }

    /// True if any path in the event is one we care about indexing.
    fn is_relevant(&self, ev: &notify::Event) -> bool {
        ev.paths.iter().any(|p| self.path_relevant(p))
    }

    fn path_relevant(&self, path: &Path) -> bool {
        // Never react to writes inside our own index directory.
        if path.starts_with(&self.base) {
            return false;
        }
        // Only consider paths inside the watched root.
        let rel = match path.strip_prefix(&self.root) {
            Ok(rel) => rel,
            Err(_) => return false,
        };

        // `is_dir` is best-effort: removed paths report `false`, which is the
        // right default for file-oriented glob matching.
        let is_dir = path.is_dir();

        if !self.index_hidden && rel.components().any(is_hidden_component) {
            return false;
        }

        if let Some(gi) = &self.gitignore {
            if gi.matched(path, is_dir).is_ignore() {
                return false;
            }
        }

        // Explicit excludes (and, when `include` is set, the whitelist) are
        // expressed as overrides exactly as in the walk.
        if self.overrides.matched(path, is_dir).is_ignore() {
            return false;
        }

        true
    }
}

fn is_hidden_component(c: std::path::Component<'_>) -> bool {
    matches!(c, std::path::Component::Normal(name)
        if name.to_str().is_some_and(|s| s.starts_with('.')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::paths::Paths;

    fn filter_for(root: &Path, config: &Config) -> Filter {
        Filter::new(&Paths::new(root), config).unwrap()
    }

    #[test]
    fn ignores_greplm_dir() {
        let root = Path::new("/proj");
        let f = filter_for(root, &Config::default());
        assert!(!f.path_relevant(&root.join(".greplm/segments/seg-000001.post")));
    }

    #[test]
    fn ignores_default_excluded_dirs() {
        let root = Path::new("/proj");
        let f = filter_for(root, &Config::default());
        // Defaults exclude .git, node_modules, target.
        assert!(!f.path_relevant(&root.join("target/debug/build.rs")));
        assert!(!f.path_relevant(&root.join(".git/index")));
        assert!(!f.path_relevant(&root.join("node_modules/foo/index.js")));
    }

    #[test]
    fn ignores_hidden_paths_by_default() {
        let root = Path::new("/proj");
        let f = filter_for(root, &Config::default());
        assert!(!f.path_relevant(&root.join(".env")));
        assert!(!f.path_relevant(&root.join(".cache/data")));
    }

    #[test]
    fn indexes_hidden_when_configured() {
        let root = Path::new("/proj");
        let config = Config {
            index_hidden: true,
            ..Config::default()
        };
        let f = filter_for(root, &config);
        assert!(f.path_relevant(&root.join(".config.toml")));
    }

    #[test]
    fn accepts_ordinary_source_files() {
        let root = Path::new("/proj");
        let f = filter_for(root, &Config::default());
        assert!(f.path_relevant(&root.join("src/main.rs")));
        assert!(f.path_relevant(&root.join("crates/core/lib.rs")));
    }

    #[test]
    fn ignores_paths_outside_root() {
        let root = Path::new("/proj");
        let f = filter_for(root, &Config::default());
        assert!(!f.path_relevant(Path::new("/other/file.rs")));
    }

    #[test]
    fn whitelist_includes_restrict_to_matching() {
        let root = Path::new("/proj");
        let config = Config {
            include: vec!["*.rs".to_string()],
            ..Config::default()
        };
        let f = filter_for(root, &config);
        assert!(f.path_relevant(&root.join("src/main.rs")));
        assert!(!f.path_relevant(&root.join("README.md")));
    }
}
