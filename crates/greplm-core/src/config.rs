//! Index configuration persisted at `.greplm/config.toml`.

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{Error, Result};

/// Which ingest read backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// Portable rayon-based read pool (all platforms).
    #[default]
    Auto,
    /// Force the portable backend.
    Rayon,
    /// Linux io_uring backend (requires the `io-uring` build feature).
    IoUring,
}

/// Persistent project configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Glob patterns to include (empty = all text files).
    pub include: Vec<String>,
    /// Extra glob patterns to exclude (on top of .gitignore).
    pub exclude: Vec<String>,
    /// Skip files larger than this many bytes.
    pub max_file_size: u64,
    /// Honor `.gitignore` / `.ignore` files during the walk.
    pub respect_gitignore: bool,
    /// Index hidden files and directories.
    pub index_hidden: bool,
    /// Ingest read backend.
    pub backend: Backend,
    /// Merge segments automatically once this many accumulate.
    pub merge_threshold: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: vec![
                "**/.git/**".to_string(),
                "**/node_modules/**".to_string(),
                "**/target/**".to_string(),
                "**/.greplm/**".to_string(),
            ],
            max_file_size: 4 * 1024 * 1024,
            respect_gitignore: true,
            index_hidden: false,
            backend: Backend::Auto,
            merge_threshold: 16,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = toml::to_string_pretty(self)?;
        std::fs::write(path, s).map_err(|e| Error::io(path, e))
    }
}
