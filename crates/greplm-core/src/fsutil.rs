//! Crash-safe file writes.
//!
//! Index files are written to a temporary sibling, flushed to disk, then
//! atomically renamed into place. A crash (or power loss) mid-write therefore
//! leaves either the old file or the new one, never a truncated/partial file
//! that a later `mmap` or deserialize would trip over.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Deterministic crash/fault injection for the atomic-write path.
///
/// This is a test seam, not a public feature: it is `#[doc(hidden)]` and a
/// no-op (one relaxed atomic load) unless explicitly armed, so it has no
/// practical cost in production. It lets the durability tests simulate a process
/// crash *just before* the Nth atomic write is published, then verify the index
/// recovers to a correct, complete state — proving the temp→fsync→rename
/// discipline and the manifest/cache recovery guards actually hold.
///
/// Once armed at index `n`, that write and every subsequent write fail, which
/// faithfully models a crash: after the process dies, no further writes happen.
#[doc(hidden)]
pub mod faults {
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

    use crate::error::{Error, Result};

    const DISABLED: i64 = -1;
    static FAIL_AT: AtomicI64 = AtomicI64::new(DISABLED);
    static COUNT: AtomicU64 = AtomicU64::new(0);

    /// Arm injection so the atomic write at zero-based index `n` — and every
    /// write after it — fails. Resets the write counter.
    pub fn arm(n: u64) {
        COUNT.store(0, Ordering::SeqCst);
        FAIL_AT.store(n as i64, Ordering::SeqCst);
    }

    /// Disable injection and reset the counter.
    pub fn disarm() {
        FAIL_AT.store(DISABLED, Ordering::SeqCst);
        COUNT.store(0, Ordering::SeqCst);
    }

    /// Number of atomic writes the guard has observed since the last arm.
    pub fn writes_seen() -> u64 {
        COUNT.load(Ordering::SeqCst)
    }

    /// Invoked at the start of each atomic write. Fast path is a single relaxed
    /// load when disarmed; only counts and compares once armed.
    pub(super) fn guard() -> Result<()> {
        if FAIL_AT.load(Ordering::Relaxed) == DISABLED {
            return Ok(());
        }
        let n = COUNT.fetch_add(1, Ordering::SeqCst) as i64;
        let target = FAIL_AT.load(Ordering::SeqCst);
        if target != DISABLED && n >= target {
            return Err(Error::other(format!("injected crash at atomic write #{n}")));
        }
        Ok(())
    }
}

/// Build a unique temporary path beside `path`.
fn tmp_path(path: &Path) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let mut name = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(format!(".tmp.{pid}.{n}"));
    match path.parent() {
        Some(dir) => dir.join(name),
        None => std::path::PathBuf::from(name),
    }
}

/// Atomically write `bytes` to `path` (write temp, fsync, rename, fsync dir).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    faults::guard()?;
    let tmp = tmp_path(path);
    let res = (|| {
        let mut f = std::fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
        f.write_all(bytes).map_err(|e| Error::io(&tmp, e))?;
        f.sync_all().map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
    })();
    match res {
        Ok(()) => {
            sync_parent_dir(path);
            Ok(())
        }
        Err(e) => {
            // Clean up the temp file on any failure (create/write/sync/rename)
            // so failed writes don't leave orphaned `.tmp.*` siblings behind.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Open a file for streaming writes at a temporary path. The handle returned by
/// [`AtomicFile::file`] is unbuffered; wrap it in a [`std::io::BufWriter`] if you
/// need buffering. Call [`AtomicFile::commit`] to fsync and rename it into place.
pub struct AtomicFile {
    final_path: std::path::PathBuf,
    tmp_path: std::path::PathBuf,
    file: Option<std::fs::File>,
}

impl AtomicFile {
    pub fn create(path: &Path) -> Result<AtomicFile> {
        let tmp = tmp_path(path);
        let file = std::fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
        Ok(AtomicFile {
            final_path: path.to_path_buf(),
            tmp_path: tmp,
            file: Some(file),
        })
    }

    /// Borrow the underlying file for writing.
    pub fn file(&mut self) -> &mut std::fs::File {
        self.file.as_mut().expect("AtomicFile already committed")
    }

    /// Flush, fsync, and atomically rename into the final path.
    pub fn commit(mut self) -> Result<()> {
        // Crash injection point: returning early here leaves `self.file` set, so
        // `Drop` cleans up the temp file — modelling a crash before the rename.
        faults::guard()?;
        // `file` is always `Some` here: `commit` takes `self` by value and
        // nothing else clears it, so the only way to reach a rename is with a
        // freshly fsynced file.
        let f = self.file.take().expect("AtomicFile already committed");
        let res = (|| {
            f.sync_all().map_err(|e| Error::io(&self.tmp_path, e))?;
            std::fs::rename(&self.tmp_path, &self.final_path)
                .map_err(|e| Error::io(&self.final_path, e))
        })();
        match res {
            Ok(()) => {
                sync_parent_dir(&self.final_path);
                Ok(())
            }
            Err(e) => {
                // We already took `file`, so `Drop` won't clean up; do it here.
                let _ = std::fs::remove_file(&self.tmp_path);
                Err(e)
            }
        }
    }
}

impl Drop for AtomicFile {
    fn drop(&mut self) {
        // If not committed, clean up the temporary file.
        if self.file.is_some() {
            let _ = std::fs::remove_file(&self.tmp_path);
        }
    }
}

/// Best-effort durability of a rename by fsyncing the containing directory.
///
/// This makes the rename durable on POSIX systems. On Windows, opening a
/// directory as a file fails, so the fsync is silently skipped and the rename's
/// durability is left to the OS; the crash-safety guarantee is therefore
/// POSIX-only (the rename itself remains atomic on both platforms).
fn sync_parent_dir(path: &Path) {
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
}
