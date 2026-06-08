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
    /// Skip files larger than this many bytes. `0` disables the cap entirely
    /// (grep parity: grep has no size limit).
    pub max_file_size: u64,
    /// Honor `.gitignore` / `.ignore` files during the walk.
    pub respect_gitignore: bool,
    /// Index hidden files and directories.
    pub index_hidden: bool,
    /// Index files containing NUL bytes (binary). Off by default; grep scans
    /// such files (`grep -a`), so enabling this restores grep parity.
    pub index_binary: bool,
    /// Index empty (zero-byte) files. Off by default since they never match.
    pub index_empty: bool,
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
            index_binary: false,
            index_empty: false,
            backend: Backend::Auto,
            merge_threshold: 16,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let mut config = match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(Error::io(path, e)),
        };
        config.apply_env_overrides();
        Ok(config)
    }

    /// Overlay `GREPLM_*` environment variables on top of the loaded config.
    /// Env wins over the file so a one-off `GREPLM_INDEX_BINARY=1 greplm index`
    /// works without editing `config.toml`. Unset or unparseable vars are
    /// ignored (the file/default value stands).
    pub fn apply_env_overrides(&mut self) {
        if let Some(v) = env_u64("GREPLM_MAX_FILE_SIZE") {
            self.max_file_size = v;
        }
        if let Some(v) = env_bool("GREPLM_RESPECT_GITIGNORE") {
            self.respect_gitignore = v;
        }
        if let Some(v) = env_bool("GREPLM_INDEX_HIDDEN") {
            self.index_hidden = v;
        }
        if let Some(v) = env_bool("GREPLM_INDEX_BINARY") {
            self.index_binary = v;
        }
        if let Some(v) = env_bool("GREPLM_INDEX_EMPTY") {
            self.index_empty = v;
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = toml::to_string_pretty(self)?;
        std::fs::write(path, s).map_err(|e| Error::io(path, e))
    }
}

/// Parse a boolean environment variable. Accepts `1/true/yes/on` and
/// `0/false/no/off` (case-insensitive); anything else (or unset) yields `None`.
fn env_bool(key: &str) -> Option<bool> {
    let raw = std::env::var(key).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Parse an unsigned-integer environment variable; unset/unparseable yields
/// `None`.
fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_grep_conservative() {
        let c = Config::default();
        assert!(!c.index_binary);
        assert!(!c.index_empty);
        assert_eq!(c.max_file_size, 4 * 1024 * 1024);
    }

    #[test]
    fn env_bool_parses_common_forms() {
        for (raw, want) in [
            ("1", true),
            ("TRUE", true),
            ("Yes", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("off", false),
        ] {
            // SAFETY: single-threaded test; key is unique per case.
            let key = format!("GREPLM_TEST_BOOL_{raw}");
            std::env::set_var(&key, raw);
            assert_eq!(env_bool(&key), Some(want), "raw={raw}");
            std::env::remove_var(&key);
        }
        assert_eq!(env_bool("GREPLM_TEST_BOOL_UNSET_XYZ"), None);
    }
}
