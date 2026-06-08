//! Ingest read path.
//!
//! Reading thousands of small source files is dominated by per-file syscall
//! overhead, not bandwidth, so the goal is high effective queue depth (many
//! reads in flight) rather than exotic zero-copy. The portable backend achieves
//! that with a rayon worker pool over buffered reads; the page cache keeps
//! re-reads (and the watch loop) cheap. An io_uring backend can be slotted in on
//! Linux behind the `io-uring` feature.

use std::path::Path;

use crate::error::{Error, Result};

/// Abstraction over how file bytes are pulled in during indexing.
pub trait IoBackend: Send + Sync {
    /// Read the full contents of a file.
    fn read(&self, path: &Path) -> Result<Vec<u8>>;

    /// Human-readable backend name (for `status`).
    fn name(&self) -> &'static str;
}

/// Portable backend: a plain buffered read. Concurrency is supplied by the
/// caller running `read` from many rayon worker threads at once.
#[derive(Debug, Default, Clone, Copy)]
pub struct RayonBackend;

impl IoBackend for RayonBackend {
    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        std::fs::read(path).map_err(|e| Error::io(path, e))
    }

    fn name(&self) -> &'static str {
        "rayon"
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
mod uring {
    use super::*;

    /// Linux io_uring backend (registered buffers + SQPOLL planned). Currently a
    /// thin placeholder that defers to a buffered read so the feature compiles
    /// and can be filled in without changing call sites.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct UringBackend;

    impl IoBackend for UringBackend {
        fn read(&self, path: &Path) -> Result<Vec<u8>> {
            std::fs::read(path).map_err(|e| Error::io(path, e))
        }

        fn name(&self) -> &'static str {
            "io_uring"
        }
    }
}

/// Select a backend based on configuration and build features.
pub fn default_backend() -> Box<dyn IoBackend> {
    Box::new(RayonBackend)
}
