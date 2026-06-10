//! Index construction: full builds, hash-gated incremental updates, compaction.

use std::collections::{HashMap, HashSet};

use rayon::prelude::*;

use crate::cache::{fast_hash, stat_key, Cache, FileRecord};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::io_backend::IoBackend;
use crate::lang::Language;
use crate::meta::{Meta, PendingTombstones};
use crate::paths::Paths;
use crate::segment::{
    merge_postings, read_bitmap, write_bitmap, write_segment_files, DocMeta, RawRef, RawSymbol,
    Segment, SegmentWriter,
};
use crate::trigram;
use crate::walk::{self, SkipReason, Skipped, WalkEntry};

/// How many skipped-file paths to retain for display. Counts in
/// `skipped_by_reason` stay exact; this only bounds the per-path sample so a
/// repo full of binaries/large files can't balloon the stats.
const SKIP_SAMPLE_CAP: usize = 100;

/// Highest segment id with files on disk, parsed from `seg-NNNNNN.*` names.
///
/// Used as a fallback when the manifest is unreadable so a rebuild keeps
/// allocating fresh ids and never overwrites a segment that the still-live
/// (pre-swap) index depends on.
fn max_segment_id_on_disk(paths: &Paths) -> Option<u64> {
    let entries = std::fs::read_dir(paths.segments_dir()).ok()?;
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            let rest = name.strip_prefix("seg-")?;
            let digits = rest.split('.').next()?;
            digits.parse::<u64>().ok()
        })
        .max()
}

/// Summary statistics returned by an index operation.
#[derive(Debug, Default, Clone)]
pub struct IndexStats {
    pub files_indexed: usize,
    /// Total files that `grep` would search but greplm left out of the index
    /// (size/empty/binary/error). Excludes gitignore/hidden pruning, which is
    /// the configured intent rather than a surprise.
    pub files_skipped: usize,
    pub files_removed: usize,
    pub symbols: usize,
    pub segments: usize,
    /// Exact count of skips grouped by reason.
    pub skipped_by_reason: std::collections::BTreeMap<SkipReason, usize>,
    /// A bounded sample of skipped paths (up to `SKIP_SAMPLE_CAP`) for display.
    pub skipped_sample: Vec<Skipped>,
}

impl IndexStats {
    /// Fold a set of skip records into the stats: bump per-reason counts, keep a
    /// bounded path sample, and set `files_skipped` to the exact total.
    fn record_skips(&mut self, skips: impl IntoIterator<Item = Skipped>) {
        for s in skips {
            *self.skipped_by_reason.entry(s.reason).or_default() += 1;
            self.files_skipped += 1;
            if self.skipped_sample.len() < SKIP_SAMPLE_CAP {
                self.skipped_sample.push(s);
            }
        }
    }
}

/// A fully processed file ready to be added to a segment.
struct Processed {
    rel: String,
    inode: u64,
    mtime_ns: i64,
    size: u64,
    hash: u64,
    doc: DocMeta,
    /// Distinct trigrams, sorted (see [`trigram::extract`]).
    trigrams: Vec<trigram::Trigram>,
    symbols: Vec<RawSymbol>,
    refs: Vec<RawRef>,
    /// Set when this file's content is byte-identical to a doc being deleted
    /// this run (a rename/move): `(segment_id, doc_id)` of the old doc, whose
    /// symbols/refs are copied instead of re-running tree-sitter.
    rename_from: Option<(u64, u32)>,
}

/// A rename-source doc: where the old copy lives plus its language, so the
/// fast path only fires when the parse result would be identical.
struct RenameSrc {
    segment_id: u64,
    doc_id: u32,
    lang: String,
}

/// Rename sources keyed by `(content hash, size)` of the deleted file.
type RenameSources = HashMap<(u64, u64), RenameSrc>;

/// Detect binary content: any NUL byte anywhere in the file.
fn is_binary(data: &[u8]) -> bool {
    memchr::memchr(0, data).is_some()
}

/// Count lines without over-counting a trailing newline as an extra empty line.
fn count_lines(data: &[u8]) -> u32 {
    if data.is_empty() {
        return 0;
    }
    let nl = memchr::memchr_iter(b'\n', data).count();
    if data.last() == Some(&b'\n') {
        nl as u32
    } else {
        (nl + 1) as u32
    }
}

/// Outcome of attempting to process a walked file.
enum Outcome {
    /// File was read and prepared for indexing.
    Indexed(Box<Processed>),
    /// File was skipped (binary or unreadable); carries the path and reason.
    Skipped(Skipped),
}

fn process(
    entry: &WalkEntry,
    backend: &dyn IoBackend,
    config: &Config,
    renames: &RenameSources,
) -> Outcome {
    let skip = |reason| {
        Outcome::Skipped(Skipped {
            rel: entry.rel.clone(),
            reason,
        })
    };
    let data = match backend.read(&entry.path) {
        Ok(d) => d,
        // A read failure (permissions, vanished mid-walk) is a skip, not a hard
        // error — record it so it's visible rather than silently dropped.
        Err(_) => return skip(SkipReason::ReadError),
    };
    if !config.index_binary && is_binary(&data) {
        return skip(SkipReason::Binary);
    }
    let ext = entry
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let lang = Language::from_extension(&ext);
    let (inode, mtime_ns, size) = stat_key(&entry.metadata);
    let hash = fast_hash(&data);
    let trigrams = trigram::extract(&data);
    // Rename fast path: identical content to a doc deleted this run, in the
    // same language, means an identical parse — skip tree-sitter and let the
    // caller copy the old doc's symbols/refs.
    let rename_from = renames
        .get(&(hash, size))
        .filter(|src| src.lang == lang.id())
        .map(|src| (src.segment_id, src.doc_id));
    let (symbols, refs) = if rename_from.is_none() && lang.grammar().is_some() {
        crate::symbol::extract_all(lang, &data)
    } else {
        (Vec::new(), Vec::new())
    };
    let doc = DocMeta {
        path: entry.rel.clone(),
        lang: lang.id().to_string(),
        size,
        hash,
        lines: count_lines(&data),
    };
    Outcome::Indexed(Box::new(Processed {
        rel: entry.rel.clone(),
        inode,
        mtime_ns,
        size,
        hash,
        doc,
        trigrams,
        symbols,
        refs,
        rename_from,
    }))
}

/// Run the read/parse stage over `candidates` in parallel, partitioning into
/// successfully processed files and skip records (binary/unreadable).
fn process_all(
    candidates: &[&WalkEntry],
    backend: &dyn IoBackend,
    config: &Config,
    renames: &RenameSources,
) -> (Vec<Processed>, Vec<Skipped>) {
    let outcomes: Vec<Outcome> = candidates
        .par_iter()
        .map(|e| process(e, backend, config, renames))
        .collect();
    let mut processed = Vec::with_capacity(outcomes.len());
    let mut skipped = Vec::new();
    for outcome in outcomes {
        match outcome {
            Outcome::Indexed(p) => processed.push(*p),
            Outcome::Skipped(s) => skipped.push(s),
        }
    }
    (processed, skipped)
}

/// Index builder bound to a project.
pub struct Indexer<'a> {
    pub paths: &'a Paths,
    pub config: &'a Config,
    pub backend: &'a dyn IoBackend,
}

impl<'a> Indexer<'a> {
    pub fn new(paths: &'a Paths, config: &'a Config, backend: &'a dyn IoBackend) -> Self {
        Self {
            paths,
            config,
            backend,
        }
    }

    /// Build the index from scratch, discarding any existing segments.
    ///
    /// The new segment is written into fresh files and the manifest is swapped
    /// atomically; the old segments are only deleted once the new index is
    /// durably published. So if the rebuild fails partway (e.g. the disk fills
    /// up), the previous index stays intact and queryable instead of being
    /// destroyed up front.
    pub fn index_full(&self) -> Result<IndexStats> {
        std::fs::create_dir_all(self.paths.segments_dir())
            .map_err(|e| Error::io(self.paths.segments_dir(), e))?;

        // Continue the segment-id counter from the existing manifest so the new
        // segment never collides with the old files we're about to replace. A
        // genuinely unreadable manifest (malformed JSON or a schema-version bump)
        // can't be trusted for the counter, so warn and recover it by scanning
        // the segments directory for the highest live id. A real IO error reading
        // the manifest is propagated rather than silently producing an empty index.
        let mut meta = match Meta::load(&self.paths.meta_file()) {
            Ok(meta) => meta,
            Err(e @ (Error::Corrupt(_) | Error::Json(_))) => {
                tracing::warn!("index manifest unusable ({e}); rebuilding from scratch");
                Meta {
                    next_segment_id: max_segment_id_on_disk(self.paths).map_or(0, |id| id + 1),
                    ..Meta::default()
                }
            }
            Err(e) => return Err(e),
        };
        let old_segments = std::mem::take(&mut meta.segments);
        // A full rebuild replaces every segment, so any unapplied tombstone
        // journal is moot.
        meta.pending_tombstones.clear();

        let cache = Cache::open(&self.paths.cache_file())?;

        let walk::WalkResult {
            entries,
            skipped: walk_skips,
        } = walk::walk(self.paths, self.config)?;
        let candidates: Vec<&WalkEntry> = entries.iter().collect();
        let (processed, proc_skips) =
            process_all(&candidates, self.backend, self.config, &RenameSources::new());

        let seg_id = meta.alloc_segment();
        let mut writer = SegmentWriter::new();
        let mut upserts = Vec::with_capacity(processed.len());
        // Consume `processed` by value: the symbol/ref tables are String-heavy
        // and cloning them per file used to dominate this loop's cost.
        for pf in processed {
            let symbols = pf.symbols.len() as u32;
            let doc_id = writer.add_doc(pf.doc, &pf.trigrams, pf.symbols, pf.refs);
            upserts.push((
                pf.rel,
                FileRecord {
                    inode: pf.inode,
                    mtime_ns: pf.mtime_ns,
                    size: pf.size,
                    hash: pf.hash,
                    segment_id: seg_id,
                    doc_id,
                    symbols,
                },
            ));
        }

        let mut stats = IndexStats {
            files_indexed: writer.doc_count(),
            files_removed: 0,
            symbols: writer.symbol_count(),
            segments: 0,
            ..Default::default()
        };
        stats.record_skips(walk_skips);
        stats.record_skips(proc_skips);

        if writer.is_empty() {
            // Nothing to index; leave an empty manifest.
            meta.segments = Vec::new();
        } else {
            writer.write(self.paths, seg_id)?;
            meta.segments = vec![seg_id];
        }
        stats.segments = meta.segments.len();

        // Publish the new manifest first so searches always see a consistent
        // index, then refresh the cache and reclaim the old segment files.
        meta.doc_count = stats.files_indexed as u64;
        meta.symbol_count = stats.symbols as u64;
        meta.record_git_head(&self.paths.root);
        meta.touch_now();
        meta.save(&self.paths.meta_file())?;

        cache.replace_all(&upserts)?;

        // Reclaim every segment file the new manifest doesn't reference — not
        // just the ids the old manifest listed. A schema-version bump (or any
        // unreadable manifest) takes the rebuild-from-scratch path above with
        // an empty `old_segments`, and trusting it would leak the entire
        // previous index on disk.
        drop(old_segments);
        self.sweep_unreferenced_segments(&meta.segments);
        Ok(stats)
    }

    /// Incrementally update the index based on filesystem changes.
    pub fn index_incremental(&self) -> Result<IndexStats> {
        let mut meta = match Meta::load(&self.paths.meta_file()) {
            Ok(meta) => meta,
            // An unreadable or outdated manifest (e.g. a greplm upgrade that
            // bumped the on-disk schema, or a truncated/corrupt meta.json) makes
            // the existing segments unusable. Rather than failing every command
            // until the user manually runs `greplm index --force`, transparently
            // rebuild from scratch — `index_full` ignores the stale manifest and
            // only swaps in the new index once it's durably written.
            Err(e @ (Error::Corrupt(_) | Error::Json(_))) => {
                tracing::warn!("index manifest unusable ({e}); rebuilding from scratch");
                return self.index_full();
            }
            Err(e) => return Err(e),
        };
        if meta.segments.is_empty() {
            return self.index_full();
        }

        // Recovery: apply any tombstones a previous run published in the
        // manifest but didn't get to write into the live bitmaps.
        self.apply_pending_tombstones(&mut meta)?;

        let cache = Cache::open(&self.paths.cache_file())?;
        let existing = cache.load_all()?;

        // Consistency guard: under normal operation every cache record points at
        // a segment listed in the manifest. If that invariant is broken (e.g. a
        // compaction that published the new manifest but was interrupted before
        // refreshing the cache), trusting the cache could tombstone the wrong
        // segment and leave duplicate or orphaned docs. The cache is rebuildable,
        // so degrade to a full rebuild instead.
        let live_segs: HashSet<u64> = meta.segments.iter().copied().collect();
        if existing
            .values()
            .any(|r| !live_segs.contains(&r.segment_id))
        {
            // Release the cache handle before `index_full` reopens the database.
            drop(existing);
            drop(cache);
            return self.index_full();
        }

        // Reverse guard: a manifest segment holding live docs that no cache
        // record references means a previous run published a delta segment but
        // crashed before its cache update landed. Trusting the cache would
        // re-index those files into duplicates, so degrade to a full rebuild.
        // (A fully tombstoned segment legitimately has no cache references and
        // is skipped by the liveness check.)
        let referenced: HashSet<u64> = existing.values().map(|r| r.segment_id).collect();
        for &seg_id in &meta.segments {
            if !referenced.contains(&seg_id)
                && read_bitmap(&self.paths.live_file(seg_id)).is_ok_and(|bm| !bm.is_empty())
            {
                drop(existing);
                drop(cache);
                return self.index_full();
            }
        }

        let walk::WalkResult {
            entries,
            skipped: walk_skips,
        } = walk::walk(self.paths, self.config)?;
        let mut seen: HashMap<String, &WalkEntry> = HashMap::with_capacity(entries.len());
        for e in &entries {
            seen.insert(e.rel.clone(), e);
        }

        // Deleted files: in the cache but no longer on disk. Computed before
        // the read/parse stage so identical-content renames can be detected
        // there and skip re-parsing.
        let deleted: Vec<String> = existing
            .keys()
            .filter(|p| !seen.contains_key(*p))
            .cloned()
            .collect();
        let (renames, rename_segs) = self.rename_sources(&existing, &deleted);

        // Decide which entries need (re)processing using a cheap stat pre-check.
        let candidates: Vec<&WalkEntry> = entries
            .iter()
            .filter(|e| {
                let (_, mtime_ns, size) = stat_key(&e.metadata);
                match existing.get(&e.rel) {
                    Some(rec) => rec.size != size || rec.mtime_ns != mtime_ns,
                    None => true,
                }
            })
            .collect();

        let (processed, proc_skips) = process_all(&candidates, self.backend, self.config, &renames);

        // Keep only entries whose content hash actually changed.
        let mut changed: Vec<Processed> = Vec::new();
        let mut touch_only: Vec<(String, FileRecord)> = Vec::new();
        for mut pf in processed {
            // Rename fast path: the parse stage skipped tree-sitter because
            // this content is identical to a doc deleted this run; copy that
            // doc's symbols/refs instead.
            if let Some((src_seg, src_doc)) = pf.rename_from {
                if let Some(seg) = rename_segs.get(&src_seg) {
                    pf.symbols = seg
                        .doc_syms(src_doc)
                        .map(|s| RawSymbol {
                            name: s.name,
                            kind: s.kind,
                            line_start: s.line_start,
                            line_end: s.line_end,
                            container: s.container,
                            signature: s.signature,
                        })
                        .collect();
                    pf.refs = seg
                        .doc_refs(src_doc)
                        .map(|r| RawRef {
                            name: r.name,
                            kind: r.kind,
                            line: r.line,
                            column: r.column,
                        })
                        .collect();
                }
            }
            match existing.get(&pf.rel) {
                Some(rec) if rec.hash == pf.hash => {
                    // Content identical; just refresh the stat key.
                    touch_only.push((
                        pf.rel.clone(),
                        FileRecord {
                            inode: pf.inode,
                            mtime_ns: pf.mtime_ns,
                            size: pf.size,
                            hash: pf.hash,
                            segment_id: rec.segment_id,
                            doc_id: rec.doc_id,
                            symbols: rec.symbols,
                        },
                    ));
                }
                _ => changed.push(pf),
            }
        }

        // Collect the docs superseded by changed and deleted files. These are
        // *not* applied yet: they are published in the manifest first (see
        // below) so adds and deletes land atomically.
        let mut tombstones: HashMap<u64, Vec<u32>> = HashMap::new();
        for pf in &changed {
            if let Some(rec) = existing.get(&pf.rel) {
                tombstones
                    .entry(rec.segment_id)
                    .or_default()
                    .push(rec.doc_id);
            }
        }
        for path in &deleted {
            if let Some(rec) = existing.get(path) {
                tombstones
                    .entry(rec.segment_id)
                    .or_default()
                    .push(rec.doc_id);
            }
        }

        // Maintain index-wide counts incrementally. Each live document maps to
        // exactly one cache record, so the deltas below keep `doc_count` /
        // `symbol_count` exact without re-opening and re-parsing every segment.
        // Computed before `changed` is consumed by the segment writer.
        let mut doc_count = meta.doc_count as i64;
        let mut sym_count = meta.symbol_count as i64;
        for path in &deleted {
            if let Some(rec) = existing.get(path) {
                doc_count -= 1;
                sym_count -= rec.symbols as i64;
            }
        }
        for pf in &changed {
            if let Some(rec) = existing.get(&pf.rel) {
                // Replaced an existing doc: drop the old, add the new.
                sym_count -= rec.symbols as i64;
            } else {
                // Brand-new file.
                doc_count += 1;
            }
            sym_count += pf.symbols.len() as i64;
        }

        let changed_count = changed.len();
        let changed_symbols: usize = changed.iter().map(|p| p.symbols.len()).sum();

        // Write changed/new files into a fresh delta segment. Only allocate a
        // segment id when there is actually something to write, so no-op
        // incrementals don't burn ids. `changed` is consumed by value so the
        // String-heavy symbol/ref tables move into the writer instead of being
        // cloned per file.
        let mut upserts = touch_only;
        if !changed.is_empty() {
            let seg_id = meta.alloc_segment();
            let mut writer = SegmentWriter::new();
            for pf in changed {
                let symbols = pf.symbols.len() as u32;
                let doc_id = writer.add_doc(pf.doc, &pf.trigrams, pf.symbols, pf.refs);
                upserts.push((
                    pf.rel,
                    FileRecord {
                        inode: pf.inode,
                        mtime_ns: pf.mtime_ns,
                        size: pf.size,
                        hash: pf.hash,
                        segment_id: seg_id,
                        doc_id,
                        symbols,
                    },
                ));
            }
            writer.write(self.paths, seg_id)?;
            meta.segments.push(seg_id);
        }

        // Atomic publish: the new delta segment *and* the doc ids it
        // supersedes land in one manifest write. Readers subtract pending
        // tombstones from the live sets they load, so the index flips from
        // old state to new state at this single rename — a crash on either
        // side never surfaces stale docs alongside their replacements.
        meta.pending_tombstones = tombstones
            .iter()
            .map(|(&segment_id, doc_ids)| PendingTombstones {
                segment_id,
                doc_ids: doc_ids.clone(),
            })
            .collect();
        meta.doc_count = doc_count.max(0) as u64;
        meta.symbol_count = sym_count.max(0) as u64;
        meta.record_git_head(&self.paths.root);
        meta.touch_now();
        meta.save(&self.paths.meta_file())?;

        // Now apply the published tombstones to the live bitmaps, refresh the
        // cache, and clear the journal. A crash anywhere in between is
        // recovered on the next run: `apply_pending_tombstones` replays the
        // journal (idempotently) and the consistency guards catch a cache that
        // never learned about the new segment.
        for (seg_id, doc_ids) in &tombstones {
            self.tombstone(*seg_id, doc_ids)?;
        }
        cache.apply(&upserts, &deleted)?;
        if !meta.pending_tombstones.is_empty() {
            meta.pending_tombstones.clear();
            meta.save(&self.paths.meta_file())?;
        }

        let mut stats = IndexStats {
            files_indexed: changed_count,
            files_removed: deleted.len(),
            symbols: changed_symbols,
            segments: meta.segments.len(),
            ..Default::default()
        };
        stats.record_skips(walk_skips);
        stats.record_skips(proc_skips);

        // Auto-compact if we've accumulated too many segments. Release the
        // cache handle first: redb allows only one open handle per process,
        // and the merge opens its own.
        if meta.segments.len() > self.config.merge_threshold {
            drop(cache);
            self.compact_auto()?;
        }
        Ok(stats)
    }

    /// Apply (and clear) any tombstones that are published in the manifest but
    /// not yet written into the per-segment live bitmaps — the recovery half
    /// of the atomic-delete protocol. Idempotent: removing an already-dead doc
    /// id is a no-op, so replaying after a crash is safe. The journal is only
    /// cleared from disk after every bitmap write succeeded.
    fn apply_pending_tombstones(&self, meta: &mut Meta) -> Result<()> {
        if meta.pending_tombstones.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut meta.pending_tombstones);
        for pt in &pending {
            // The segment may have been dropped by a later operation.
            if meta.segments.contains(&pt.segment_id) {
                self.tombstone(pt.segment_id, &pt.doc_ids)?;
            }
        }
        meta.save(&self.paths.meta_file())
    }

    /// Build the rename-source table for this run: for every file that
    /// disappeared from disk, map its `(content hash, size)` to the old doc so
    /// a new path with byte-identical content (a rename/move) can copy the old
    /// doc's symbols and refs instead of re-running tree-sitter. Each source
    /// segment is opened once; an unopenable segment just disables the fast
    /// path for its docs (the slow path re-parses).
    fn rename_sources(
        &self,
        existing: &HashMap<String, FileRecord>,
        deleted: &[String],
    ) -> (RenameSources, HashMap<u64, Segment>) {
        let mut wanted: HashMap<u64, Vec<&FileRecord>> = HashMap::new();
        for path in deleted {
            if let Some(rec) = existing.get(path) {
                wanted.entry(rec.segment_id).or_default().push(rec);
            }
        }
        let mut renames = RenameSources::new();
        let mut segs: HashMap<u64, Segment> = HashMap::new();
        for (seg_id, recs) in wanted {
            let seg = match Segment::open(self.paths, seg_id) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!("rename fast-path disabled for segment {seg_id}: {e}");
                    continue;
                }
            };
            for rec in recs {
                if let Some(doc) = seg.doc(rec.doc_id) {
                    renames.insert(
                        (rec.hash, rec.size),
                        RenameSrc {
                            segment_id: seg_id,
                            doc_id: rec.doc_id,
                            lang: doc.lang.clone(),
                        },
                    );
                }
            }
            segs.insert(seg_id, seg);
        }
        (renames, segs)
    }

    /// Merge all live documents from every segment into a single compact
    /// segment. This reuses the already-indexed postings/symbols (no file reads,
    /// no re-parsing) and falls back to a full rebuild if anything goes wrong.
    pub fn compact(&self) -> Result<IndexStats> {
        match self.merge_segments(true) {
            Ok(stats) => Ok(stats),
            Err(e) => {
                tracing::warn!("merge compaction failed ({e}); falling back to full rebuild");
                self.index_full()
            }
        }
    }

    /// Auto-compaction (tiered): merge only the *smallest* segments — by live
    /// doc count — down to half the merge threshold, leaving the large ones
    /// untouched. Compared to rewriting the whole index on every threshold
    /// crossing, each doc is rewritten O(log n) times over the index's life
    /// instead of O(n / threshold).
    fn compact_auto(&self) -> Result<IndexStats> {
        match self.merge_segments(false) {
            Ok(stats) => Ok(stats),
            Err(e) => {
                tracing::warn!("auto compaction failed ({e}); falling back to full rebuild");
                self.index_full()
            }
        }
    }

    /// Core of [`compact`] / [`Self::compact_auto`]: a streaming k-way merge
    /// over the chosen segments. Doc/symbol/ref tables are concatenated with
    /// remapped ids; postings are merged by a union over the segments' FST
    /// term dictionaries, so the merged index's posting lists are never all
    /// resident at once.
    fn merge_segments(&self, all: bool) -> Result<IndexStats> {
        let mut meta = Meta::load(&self.paths.meta_file())?;
        if meta.segments.is_empty() {
            return Ok(IndexStats::default());
        }
        // Live bitmaps are about to be read; make sure published-but-unapplied
        // deletes are honored first.
        self.apply_pending_tombstones(&mut meta)?;

        let cache = Cache::open(&self.paths.cache_file())?;
        let existing = cache.load_all()?;

        // Victim selection: everything for an explicit compact; otherwise the
        // smallest segments, leaving `target` slots for the survivors plus the
        // merged output.
        let victim_ids: Vec<u64> = if all {
            meta.segments.clone()
        } else {
            let target = (self.config.merge_threshold / 2).max(1);
            if meta.segments.len() <= target {
                return Ok(IndexStats::default());
            }
            let mut by_live: Vec<(u64, u64)> = meta
                .segments
                .iter()
                .map(|&id| Ok((id, read_bitmap(&self.paths.live_file(id))?.len())))
                .collect::<Result<_>>()?;
            by_live.sort_by_key(|&(_, n)| n);
            by_live.truncate(meta.segments.len() - target + 1);
            by_live.into_iter().map(|(id, _)| id).collect()
        };
        let victims: HashSet<u64> = victim_ids.iter().copied().collect();

        let segments: Vec<Segment> = victim_ids
            .iter()
            .map(|&id| Segment::open(self.paths, id))
            .collect::<Result<_>>()?;

        let mut docs: Vec<DocMeta> = Vec::new();
        // Rows stream straight into the columnar builders; the merged tables
        // are never materialized as entry vecs.
        let mut syms = crate::table::SymTableBuilder::new();
        let mut refs = crate::table::RefTableBuilder::new();
        let mut remaps: Vec<Vec<u32>> = Vec::with_capacity(segments.len());
        let mut upserts: Vec<(String, FileRecord)> = Vec::new();
        let new_seg_id = meta.alloc_segment();

        for seg in &segments {
            // Old doc id -> new doc id; `u32::MAX` marks tombstoned docs.
            let mut remap: Vec<u32> = vec![u32::MAX; seg.docs.len()];
            for old_id in seg.all_live().iter() {
                let doc = match seg.doc(old_id) {
                    Some(d) => d,
                    None => continue,
                };
                let new_id = docs.len() as u32;
                remap[old_id as usize] = new_id;
                let syms_before = syms.len();
                for s in seg.doc_syms(old_id) {
                    syms.push(
                        new_id,
                        &s.name,
                        &s.kind,
                        s.line_start,
                        s.line_end,
                        s.container.as_deref(),
                        s.signature.as_deref(),
                    )?;
                }
                for r in seg.doc_refs(old_id) {
                    refs.push(new_id, &r.name, r.kind, r.line, r.column)?;
                }
                let doc_sym_count = (syms.len() - syms_before) as u32;
                let (inode, mtime_ns) = existing
                    .get(&doc.path)
                    .map(|r| (r.inode, r.mtime_ns))
                    .unwrap_or((0, 0));
                upserts.push((
                    doc.path.clone(),
                    FileRecord {
                        inode,
                        mtime_ns,
                        size: doc.size,
                        hash: doc.hash,
                        segment_id: new_seg_id,
                        doc_id: new_id,
                        symbols: doc_sym_count,
                    },
                ));
                docs.push(doc.clone());
            }
            remaps.push(remap);
        }

        let symbol_count = syms.len();
        let doc_count = docs.len();

        // The streaming postings merge and the table finishes (each ending in
        // a parallel name sort) are independent; overlap them.
        let (postings, tables) = rayon::join(
            || merge_postings(&segments, &remaps),
            || {
                rayon::join(
                    || syms.finish(doc_count),
                    || refs.finish(doc_count),
                )
            },
        );
        let (post_blob, fst_entries) = postings?;
        let (syms_enc, refs_enc) = tables;
        drop(segments);

        meta.segments.retain(|id| !victims.contains(id));
        if !docs.is_empty() {
            write_segment_files(
                self.paths,
                new_seg_id,
                &docs,
                syms_enc?,
                refs_enc?,
                &fst_entries,
                post_blob,
            )?;
            meta.segments.push(new_seg_id);
        }

        // Publish the new manifest first so searches always see a consistent
        // index, then refresh the cache and reclaim the old segment files.
        if all {
            // A full merge sees every live doc, so the totals are exact.
            meta.doc_count = doc_count as u64;
            meta.symbol_count = symbol_count as u64;
        }
        meta.touch_now();
        meta.save(&self.paths.meta_file())?;

        if all {
            cache.replace_all(&upserts)?;
        } else {
            // Survivor segments keep their cache records; only merged docs
            // move.
            cache.apply(&upserts, &[])?;
        }

        for id in victim_ids {
            self.remove_segment_files(id);
        }

        Ok(IndexStats {
            files_indexed: doc_count,
            files_removed: 0,
            symbols: symbol_count,
            segments: meta.segments.len(),
            ..Default::default()
        })
    }

    /// Best-effort removal of every `seg-*` file whose id is not in `live`.
    /// Safe against concurrent readers: they hold mmaps/open fds, so unlink
    /// only reclaims the space once they drop the segment.
    fn sweep_unreferenced_segments(&self, live: &[u64]) {
        let Ok(entries) = std::fs::read_dir(self.paths.segments_dir()) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|n| n.strip_prefix("seg-"))
                .and_then(|rest| rest.split('.').next())
                .and_then(|digits| digits.parse::<u64>().ok())
            else {
                continue;
            };
            if !live.contains(&id) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// Best-effort removal of all files belonging to a segment id.
    fn remove_segment_files(&self, seg_id: u64) {
        for path in [
            self.paths.fst_file(seg_id),
            self.paths.post_file(seg_id),
            self.paths.docs_file(seg_id),
            self.paths.syms_file(seg_id),
            self.paths.refs_file(seg_id),
            self.paths.live_file(seg_id),
        ] {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Clear a set of doc ids from a segment's live bitmap.
    fn tombstone(&self, seg_id: u64, doc_ids: &[u32]) -> Result<()> {
        let live_path = self.paths.live_file(seg_id);
        let mut live = read_bitmap(&live_path)?;
        for id in doc_ids {
            live.remove(*id);
        }
        write_bitmap(&live_path, &live)
    }
}
