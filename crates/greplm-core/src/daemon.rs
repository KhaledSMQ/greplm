//! Warm-index daemon.
//!
//! Holds the index mmapped in memory with the filesystem watcher running, so
//! query latency drops to the cost of the query itself (no per-invocation open +
//! mmap + table load). Clients talk to it over a Unix domain socket.

#[cfg(unix)]
pub use unix_impl::serve;

#[cfg(not(unix))]
pub use stub_impl::serve;

// The daemon relies on Unix domain sockets and is unavailable on other
// platforms. The stub keeps the CLI compiling everywhere and reports a clear
// error if `greplm serve` is invoked.
#[cfg(not(unix))]
mod stub_impl {
    use std::path::Path;
    use std::sync::Arc;

    use crate::error::{Error, Result};
    use crate::Greplm;

    /// Unsupported on this platform: the daemon requires Unix domain sockets.
    pub fn serve(_greplm: Arc<Greplm>, _socket: &Path) -> Result<()> {
        Err(Error::other(
            "greplm daemon is not supported on this platform",
        ))
    }
}

#[cfg(unix)]
mod unix_impl {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use crate::error::{Error, Result};
    use crate::proto::{Request, Response};
    use crate::search::Searcher;
    use crate::Greplm;

    type Shared = Arc<RwLock<Searcher>>;

    /// Maximum size of a single request line; protects against unbounded memory
    /// growth from a malformed or hostile client.
    const MAX_REQUEST_BYTES: u64 = 1 << 20; // 1 MiB

    /// Maximum number of clients served concurrently. Excess connections are
    /// rejected rather than spawning unbounded threads.
    const MAX_CONNECTIONS: usize = 256;

    static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

    /// RAII guard that tracks the live connection count.
    struct ConnGuard;
    impl Drop for ConnGuard {
        fn drop(&mut self) {
            ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Recover the inner value from a poisoned lock instead of propagating the
    /// poison; a panicked query must not permanently disable the daemon.
    fn read_searcher(s: &Shared) -> std::sync::RwLockReadGuard<'_, Searcher> {
        s.read().unwrap_or_else(|e| e.into_inner())
    }

    fn swap_searcher(s: &Shared, new: Searcher) {
        let mut guard = s.write().unwrap_or_else(|e| e.into_inner());
        *guard = new;
    }

    /// Run the daemon: build/refresh the index, start the watcher, and serve
    /// clients on `socket` until the process is terminated.
    pub fn serve(greplm: Arc<Greplm>, socket: &Path) -> Result<()> {
        greplm.ensure_initialized()?;
        greplm.index(false)?;
        let searcher: Shared = Arc::new(RwLock::new(greplm.searcher()?));

        // Background watcher: reindex incrementally and hot-swap the searcher.
        // If the watcher dies, log and restart it after a short backoff so the
        // index doesn't silently stop updating.
        {
            let g_watch = greplm.clone();
            let s = searcher.clone();
            std::thread::Builder::new()
                .name("greplm-watch".into())
                .spawn(move || loop {
                    let g_cb = g_watch.clone();
                    let s_cb = s.clone();
                    let result = g_watch.watch(Duration::from_millis(300), move |_stats| {
                        if let Ok(ns) = g_cb.searcher() {
                            swap_searcher(&s_cb, ns);
                        }
                    });
                    match result {
                        Ok(()) => break,
                        Err(e) => {
                            tracing::warn!("watcher stopped ({e}); restarting in 1s");
                            std::thread::sleep(Duration::from_secs(1));
                        }
                    }
                })
                .ok();
        }

        // Fresh socket each run.
        if socket.exists() {
            let _ = std::fs::remove_file(socket);
        }
        let listener = UnixListener::bind(socket).map_err(|e| Error::io(socket, e))?;
        // Restrict the socket to the owner so other local users can't connect
        // and issue queries (which can read indexed file contents) as us.
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(e) = std::fs::set_permissions(socket, perms) {
                tracing::warn!("could not restrict socket permissions: {e}");
            }
        }
        tracing::info!("greplm daemon listening on {}", socket.display());

        for conn in listener.incoming() {
            match conn {
                Ok(mut stream) => {
                    // Reject excess connections instead of spawning unbounded
                    // threads; the guard decrements the count when the handler
                    // finishes.
                    let prev = ACTIVE_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
                    if prev >= MAX_CONNECTIONS {
                        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
                        let resp = Response::err("server busy: too many connections");
                        if let Ok(mut bytes) = serde_json::to_vec(&resp) {
                            bytes.push(b'\n');
                            let _ = stream.write_all(&bytes);
                        }
                        continue;
                    }
                    let s = searcher.clone();
                    let g = greplm.clone();
                    std::thread::spawn(move || {
                        let _guard = ConnGuard;
                        if let Err(e) = handle(stream, s, g) {
                            tracing::debug!("client error: {e}");
                        }
                    });
                }
                Err(e) => tracing::debug!("accept error: {e}"),
            }
        }
        Ok(())
    }

    fn handle(stream: UnixStream, searcher: Shared, greplm: Arc<Greplm>) -> Result<()> {
        let mut reader = BufReader::new(stream.try_clone().map_err(Error::PlainIo)?);
        let mut writer = stream;
        let mut line = String::new();
        loop {
            line.clear();
            // Bound the request size so a client can't make us buffer unbounded
            // memory on a single line.
            let n = (&mut reader)
                .take(MAX_REQUEST_BYTES)
                .read_line(&mut line)
                .map_err(Error::PlainIo)?;
            if n == 0 {
                break; // client disconnected
            }
            if n as u64 >= MAX_REQUEST_BYTES && !line.ends_with('\n') {
                let resp = Response::err("request too large");
                let mut bytes = serde_json::to_vec(&resp)?;
                bytes.push(b'\n');
                writer.write_all(&bytes).map_err(Error::PlainIo)?;
                writer.flush().map_err(Error::PlainIo)?;
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let resp = match serde_json::from_str::<Request>(trimmed) {
                Ok(req) => {
                    // Isolate each query: a panic while handling one request
                    // (a bug, an arithmetic overflow, a corrupt segment) must
                    // not poison the shared searcher beyond recovery or drop
                    // the connection — return an error to this client instead.
                    let s = &searcher;
                    let g = &greplm;
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(req, s, g)))
                        .unwrap_or_else(|_| Response::err("internal error: query panicked"))
                }
                Err(e) => Response::err(format!("bad request: {e}")),
            };
            let mut bytes = serde_json::to_vec(&resp)?;
            bytes.push(b'\n');
            writer.write_all(&bytes).map_err(Error::PlainIo)?;
            writer.flush().map_err(Error::PlainIo)?;
        }
        Ok(())
    }

    fn dispatch(req: Request, searcher: &Shared, greplm: &Arc<Greplm>) -> Response {
        let json = |v| Response::ok(v);
        match req {
            Request::Ping => Response::ok(serde_json::json!({"pong": true})),
            Request::Status => match greplm.status() {
                Ok(s) => to_resp(serde_json::to_value(s)),
                Err(e) => Response::err(e.to_string()),
            },
            Request::Reindex { force } => match greplm.index(force) {
                Ok(stats) => {
                    if let Ok(ns) = greplm.searcher() {
                        swap_searcher(searcher, ns);
                    }
                    json(serde_json::json!({
                        "files_indexed": stats.files_indexed,
                        "files_removed": stats.files_removed,
                        "symbols": stats.symbols,
                        "segments": stats.segments,
                    }))
                }
                Err(e) => Response::err(e.to_string()),
            },
            other => {
                let guard = read_searcher(searcher);
                match other {
                    Request::Summary => to_resp(serde_json::to_value(guard.summary())),
                    Request::Search(q) => match guard.search(&q) {
                        Ok(h) => to_resp(serde_json::to_value(h)),
                        Err(e) => Response::err(e.to_string()),
                    },
                    Request::Symbols(q) => match guard.symbols(&q) {
                        Ok(h) => to_resp(serde_json::to_value(h)),
                        Err(e) => Response::err(e.to_string()),
                    },
                    Request::Refs {
                        name,
                        limit,
                        offset,
                    } => match guard.references(&name, limit, offset) {
                        Ok(h) => to_resp(serde_json::to_value(h)),
                        Err(e) => Response::err(e.to_string()),
                    },
                    Request::RefsResolved {
                        name,
                        limit,
                        offset,
                    } => to_resp(serde_json::to_value(
                        guard.references_resolved(&name, limit, offset),
                    )),
                    Request::Callers {
                        name,
                        limit,
                        offset,
                    } => to_resp(serde_json::to_value(guard.callers(&name, limit, offset))),
                    Request::Callees {
                        name,
                        limit,
                        offset,
                    } => to_resp(serde_json::to_value(guard.callees(&name, limit, offset))),
                    Request::BlastRadius { name, depth, limit } => to_resp(serde_json::to_value(
                        guard.blast_radius(&name, depth, limit),
                    )),
                    Request::Definition { file, line, col } => {
                        match guard.definition(&file, line, col) {
                            Ok(h) => to_resp(serde_json::to_value(h)),
                            Err(e) => Response::err(e.to_string()),
                        }
                    }
                    Request::ReferencesAt { file, line, col } => {
                        match guard.references_of(&file, line, col) {
                            Ok(h) => to_resp(serde_json::to_value(h)),
                            Err(e) => Response::err(e.to_string()),
                        }
                    }
                    Request::Structural {
                        pattern,
                        lang,
                        limit,
                        offset,
                    } => match guard.structural_search(&pattern, &lang, limit, offset) {
                        Ok(h) => to_resp(serde_json::to_value(h)),
                        Err(e) => Response::err(e.to_string()),
                    },
                    Request::ContextPack { task, budget } => {
                        to_resp(serde_json::to_value(guard.context_pack(&task, budget)))
                    }
                    Request::Blame { file, line } => match guard.blame(&file, line) {
                        Ok(b) => to_resp(serde_json::to_value(b)),
                        Err(e) => Response::err(e.to_string()),
                    },
                    Request::History { name, limit } => match guard.symbol_history(&name, limit) {
                        Ok(h) => to_resp(serde_json::to_value(h)),
                        Err(e) => Response::err(e.to_string()),
                    },
                    Request::ChangedSince { rev } => match guard.changed_since(&rev) {
                        Ok(c) => to_resp(serde_json::to_value(c)),
                        Err(e) => Response::err(e.to_string()),
                    },
                    Request::Outline { file } => match guard.outline(&file) {
                        Ok(h) => to_resp(serde_json::to_value(h)),
                        Err(e) => Response::err(e.to_string()),
                    },
                    Request::Snippet {
                        file,
                        start,
                        end,
                        context,
                    } => match guard.read_snippet(&file, start, end, context) {
                        Ok(h) => to_resp(serde_json::to_value(h)),
                        Err(e) => Response::err(e.to_string()),
                    },
                    // Handled above.
                    Request::Ping | Request::Status | Request::Reindex { .. } => {
                        Response::err("unreachable")
                    }
                }
            }
        }
    }

    fn to_resp(v: serde_json::Result<serde_json::Value>) -> Response {
        match v {
            Ok(value) => Response::ok(value),
            Err(e) => Response::err(e.to_string()),
        }
    }
}
