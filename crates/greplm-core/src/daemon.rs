//! Warm-index daemon.
//!
//! Holds the index mmapped in memory with the filesystem watcher running, so
//! query latency drops to the cost of the query itself (no per-invocation open +
//! mmap + table load). Clients talk to it over a Unix domain socket.

#[cfg(unix)]
pub use unix_impl::{serve, serve_global};

#[cfg(not(unix))]
pub use stub_impl::{serve, serve_global};

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

    /// Unsupported on this platform: the daemon requires Unix domain sockets.
    pub fn serve_global(_socket: &Path) -> Result<()> {
        Err(Error::other(
            "greplm daemon is not supported on this platform",
        ))
    }
}

#[cfg(unix)]
mod unix_impl {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use arc_swap::ArcSwap;

    use crate::error::{Error, Result};
    use crate::proto::{Request, Response, RoutedRequest};
    use crate::search::Searcher;
    use crate::{watch, Greplm};

    /// The shared, hot-swappable searcher. `ArcSwap` makes reads wait-free
    /// (queries never block on a swap, and a swap never waits for in-flight
    /// queries) and cannot be poisoned by a panicking query thread.
    type Shared = Arc<ArcSwap<Searcher>>;

    /// Maximum size of a single request line; protects against unbounded memory
    /// growth from a malformed or hostile client.
    const MAX_REQUEST_BYTES: u64 = 1 << 20; // 1 MiB

    /// Maximum number of clients served concurrently. Excess connections are
    /// rejected rather than spawning unbounded threads.
    const MAX_CONNECTIONS: usize = 256;

    static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

    /// Debounce window for the background watcher. Short enough to give prompt
    /// read-after-write freshness (an edit is reflected within ~this latency)
    /// while still coalescing editor write-bursts into one re-index.
    const WATCH_DEBOUNCE: Duration = Duration::from_millis(100);

    /// RAII guard that tracks the live connection count.
    struct ConnGuard;
    impl Drop for ConnGuard {
        fn drop(&mut self) {
            ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Rebuild the searcher (reusing unchanged segments and the warm content
    /// cache of the current one) and publish it. Reads concurrently in flight
    /// keep their old snapshot; new reads see the fresh index.
    fn refresh_searcher(s: &Shared, greplm: &Greplm) {
        let prev = s.load_full();
        match greplm.searcher_reusing(&prev) {
            Ok(ns) => s.store(Arc::new(ns)),
            Err(e) => tracing::warn!("searcher refresh failed: {e}"),
        }
    }

    /// Run the daemon: build/refresh the index, start the watcher, and serve
    /// clients on `socket` until the process is terminated.
    pub fn serve(greplm: Arc<Greplm>, socket: &Path) -> Result<()> {
        greplm.ensure_initialized()?;
        greplm.index(false)?;
        let searcher: Shared = Arc::new(ArcSwap::from_pointee(greplm.searcher()?));

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
                    let result = g_watch.watch(WATCH_DEBOUNCE, move |_stats| {
                        refresh_searcher(&s_cb, &g_cb);
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
        match req {
            Request::Ping => Response::json(&serde_json::json!({"pong": true})),
            Request::Status => match greplm.status() {
                Ok(s) => Response::json(&s),
                Err(e) => Response::err(e.to_string()),
            },
            Request::Reindex { force } => match greplm.index(force) {
                Ok(stats) => {
                    refresh_searcher(searcher, greplm);
                    Response::json(&serde_json::json!({
                        "files_indexed": stats.files_indexed,
                        "files_removed": stats.files_removed,
                        "symbols": stats.symbols,
                        "segments": stats.segments,
                    }))
                }
                Err(e) => Response::err(e.to_string()),
            },
            other => {
                // Freshness comes from the background watcher (event-driven,
                // ~debounce latency), not a per-query filesystem poll: walking
                // the tree + opening the cache on every read cost ~140ms and
                // defeated the warm daemon. The watcher hot-swaps the searcher
                // on change, so reads stay ~sub-ms and reflect edits within the
                // debounce window.
                let guard = searcher.load();
                match other {
                    Request::Summary => Response::json(&guard.summary()),
                    Request::Search(q) => to_resp(guard.search(&q)),
                    Request::Symbols(q) => to_resp(guard.symbols(&q)),
                    Request::Refs {
                        name,
                        limit,
                        offset,
                    } => to_resp(guard.references(&name, limit, offset)),
                    Request::RefsResolved {
                        name,
                        limit,
                        offset,
                    } => Response::json(&guard.references_resolved(&name, limit, offset)),
                    Request::Callers {
                        name,
                        limit,
                        offset,
                    } => Response::json(&guard.callers(&name, limit, offset)),
                    Request::Callees {
                        name,
                        limit,
                        offset,
                    } => Response::json(&guard.callees(&name, limit, offset)),
                    Request::BlastRadius { name, depth, limit } => {
                        Response::json(&guard.blast_radius(&name, depth, limit))
                    }
                    Request::Definition { file, line, col } => {
                        to_resp(guard.definition(&file, line, col))
                    }
                    Request::ReferencesAt { file, line, col } => {
                        to_resp(guard.references_of(&file, line, col))
                    }
                    Request::Structural {
                        pattern,
                        lang,
                        limit,
                        offset,
                    } => to_resp(guard.structural_search(&pattern, &lang, limit, offset)),
                    Request::ContextPack { task, budget } => {
                        Response::json(&guard.context_pack(&task, budget))
                    }
                    Request::Blame { file, line } => to_resp(guard.blame(&file, line)),
                    Request::History { name, limit } => to_resp(guard.symbol_history(&name, limit)),
                    Request::ChangedSince { rev } => to_resp(guard.changed_since(&rev)),
                    Request::Outline { file } => to_resp(guard.outline(&file)),
                    Request::Snippet {
                        file,
                        start,
                        end,
                        context,
                    } => to_resp(guard.read_snippet(&file, start, end, context)),
                    // Handled above.
                    Request::Ping | Request::Status | Request::Reindex { .. } => {
                        Response::err("unreachable")
                    }
                }
            }
        }
    }

    /// Serialize a query result once, mapping query errors to error responses.
    fn to_resp<T: serde::Serialize>(v: Result<T>) -> Response {
        match v {
            Ok(value) => Response::json(&value),
            Err(e) => Response::err(e.to_string()),
        }
    }

    // ---- Global multi-root daemon -------------------------------------------
    //
    // One process serves every project the user touches, over a single
    // machine-wide socket. Projects are loaded lazily on first query (each gets
    // a warm in-memory index + its own watcher) and evicted after an idle
    // period, so running many agents across many repos costs one background
    // process whose memory tracks only the projects in active use.

    /// Evict a project's index + watcher after this long with no queries.
    const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
    /// How often the reaper scans for idle projects.
    const EVICT_INTERVAL: Duration = Duration::from_secs(60);

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// A single warm project held by the global daemon.
    struct Entry {
        greplm: Arc<Greplm>,
        searcher: Shared,
        /// Unix seconds of the last query; drives idle eviction.
        last_used: AtomicU64,
        /// Set on eviction to stop this project's watcher thread.
        stop: Arc<AtomicBool>,
    }

    impl Entry {
        fn touch(&self) {
            self.last_used.store(now_secs(), Ordering::Relaxed);
        }
    }

    type Registry = Arc<RwLock<HashMap<PathBuf, Arc<Entry>>>>;

    /// Get the warm entry for `root`, lazily loading (index + watcher) on first
    /// use. The first query for a project pays the index-open cost; subsequent
    /// queries are served warm until the project is evicted for being idle.
    fn get_or_load(reg: &Registry, root: &Path) -> Result<Arc<Entry>> {
        if let Some(e) = reg
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(root)
            .cloned()
        {
            e.touch();
            return Ok(e);
        }
        let mut w = reg.write().unwrap_or_else(|e| e.into_inner());
        // Double-check: another thread may have loaded it while we waited.
        if let Some(e) = w.get(root).cloned() {
            e.touch();
            return Ok(e);
        }

        let greplm = Arc::new(Greplm::open(root)?);
        greplm.ensure_indexed()?;
        let searcher: Shared = Arc::new(ArcSwap::from_pointee(greplm.searcher()?));
        let stop = Arc::new(AtomicBool::new(false));

        // Per-project watcher: hot-swap this entry's searcher on change, exit
        // when the entry is evicted (stop flag).
        {
            let g_watch = greplm.clone();
            let g_cb = greplm.clone();
            let s = searcher.clone();
            let stop = stop.clone();
            let root_disp = root.to_path_buf();
            std::thread::Builder::new()
                .name("greplm-watch".into())
                .spawn(move || {
                    let r = watch::run_cancellable(&g_watch, WATCH_DEBOUNCE, stop, move |_stats| {
                        refresh_searcher(&s, &g_cb);
                    });
                    if let Err(e) = r {
                        tracing::warn!("watcher for {} stopped: {e}", root_disp.display());
                    }
                })
                .ok();
        }

        let entry = Arc::new(Entry {
            greplm,
            searcher,
            last_used: AtomicU64::new(now_secs()),
            stop,
        });
        w.insert(root.to_path_buf(), entry.clone());
        tracing::info!("loaded project {} ({} warm)", root.display(), w.len());
        Ok(entry)
    }

    /// Background reaper: drop projects idle longer than [`IDLE_TIMEOUT`],
    /// stopping their watcher and freeing their index.
    fn spawn_reaper(reg: Registry) {
        std::thread::Builder::new()
            .name("greplm-reaper".into())
            .spawn(move || loop {
                std::thread::sleep(EVICT_INTERVAL);
                let now = now_secs();
                let mut w = reg.write().unwrap_or_else(|e| e.into_inner());
                let stale: Vec<PathBuf> = w
                    .iter()
                    .filter(|(_, e)| {
                        now.saturating_sub(e.last_used.load(Ordering::Relaxed))
                            >= IDLE_TIMEOUT.as_secs()
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in stale {
                    if let Some(e) = w.remove(&k) {
                        e.stop.store(true, Ordering::Relaxed); // watcher exits; index frees on last Arc drop
                        tracing::info!("evicted idle project {}", k.display());
                    }
                }
            })
            .ok();
    }

    /// Run the global multi-root daemon on `socket` until terminated.
    pub fn serve_global(socket: &Path) -> Result<()> {
        if let Some(dir) = socket.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        if socket.exists() {
            let _ = std::fs::remove_file(socket);
        }
        let listener = UnixListener::bind(socket).map_err(|e| Error::io(socket, e))?;
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
            {
                tracing::warn!("could not restrict socket permissions: {e}");
            }
        }

        let registry: Registry = Arc::new(RwLock::new(HashMap::new()));
        spawn_reaper(registry.clone());
        tracing::info!("greplm global daemon listening on {}", socket.display());

        for conn in listener.incoming() {
            match conn {
                Ok(mut stream) => {
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
                    let reg = registry.clone();
                    std::thread::spawn(move || {
                        let _guard = ConnGuard;
                        if let Err(e) = handle_global(stream, reg) {
                            tracing::debug!("client error: {e}");
                        }
                    });
                }
                Err(e) => tracing::debug!("accept error: {e}"),
            }
        }
        Ok(())
    }

    fn handle_global(stream: UnixStream, reg: Registry) -> Result<()> {
        let mut reader = BufReader::new(stream.try_clone().map_err(Error::PlainIo)?);
        let mut writer = stream;
        let mut line = String::new();
        loop {
            line.clear();
            let n = (&mut reader)
                .take(MAX_REQUEST_BYTES)
                .read_line(&mut line)
                .map_err(Error::PlainIo)?;
            if n == 0 {
                break;
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
            let resp = match serde_json::from_str::<RoutedRequest>(trimmed) {
                Ok(routed) => {
                    let reg = &reg;
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        dispatch_global(routed, reg)
                    }))
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

    fn dispatch_global(routed: RoutedRequest, reg: &Registry) -> Response {
        let entry = match get_or_load(reg, &routed.root) {
            Ok(e) => e,
            Err(e) => return Response::err(e.to_string()),
        };
        // Reuse the per-project dispatcher against this project's warm index.
        dispatch(routed.req, &entry.searcher, &entry.greplm)
    }
}
