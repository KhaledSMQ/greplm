use std::path::{Path, PathBuf};

/// The name of the directory where greplm stores its index and cache.
pub const DIR_NAME: &str = ".greplm";

/// Filesystem layout for a single indexed project.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Project root (the directory being indexed).
    pub root: PathBuf,
    /// The `.greplm` directory.
    pub base: PathBuf,
}

impl Paths {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let base = root.join(DIR_NAME);
        Self { root, base }
    }

    /// Directory holding immutable index segments.
    pub fn segments_dir(&self) -> PathBuf {
        self.base.join("segments")
    }

    pub fn config_file(&self) -> PathBuf {
        self.base.join("config.toml")
    }

    pub fn meta_file(&self) -> PathBuf {
        self.base.join("meta.json")
    }

    pub fn cache_file(&self) -> PathBuf {
        self.base.join("cache.redb")
    }

    /// Append-only log of per-query token-savings records.
    pub fn savings_file(&self) -> PathBuf {
        self.base.join("savings.jsonl")
    }

    pub fn gitignore_file(&self) -> PathBuf {
        self.base.join(".gitignore")
    }

    pub fn fst_file(&self, seg: u64) -> PathBuf {
        self.segments_dir().join(format!("seg-{seg:06}.fst"))
    }

    pub fn post_file(&self, seg: u64) -> PathBuf {
        self.segments_dir().join(format!("seg-{seg:06}.post"))
    }

    pub fn docs_file(&self, seg: u64) -> PathBuf {
        self.segments_dir().join(format!("seg-{seg:06}.docs"))
    }

    pub fn syms_file(&self, seg: u64) -> PathBuf {
        self.segments_dir().join(format!("seg-{seg:06}.syms"))
    }

    pub fn refs_file(&self, seg: u64) -> PathBuf {
        self.segments_dir().join(format!("seg-{seg:06}.refs"))
    }

    pub fn live_file(&self, seg: u64) -> PathBuf {
        self.segments_dir().join(format!("seg-{seg:06}.live"))
    }

    /// True if an index directory exists.
    pub fn exists(&self) -> bool {
        self.base.is_dir()
    }
}
