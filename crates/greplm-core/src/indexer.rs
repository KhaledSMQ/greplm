//! Index construction: full builds, hash-gated incremental updates, compaction.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rayon::prelude::*;
use roaring::RoaringBitmap;

use crate::cache::{fast_hash, stat_key, Cache, FileRecord};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::io_backend::IoBackend;
use crate::lang::Language;
use crate::meta::Meta;
use crate::paths::Paths;
use crate::segment::{
    read_bitmap, write_bitmap, write_segment_from_parts, DocMeta, RawRef, RawSymbol, RefEntry,
    Segment, SegmentWriter, SymbolEntry,
};
use crate::trigram::{self, Trigram};
use crate::walk::{self, SkipReason, Skipped, WalkEntry};

/// How many skipped-file paths to retain for display. Counts in
/// `skipped_by_reason` stay exact; this only bounds the per-path sample so a
/// repo full of binaries/large files can't balloon the stats.
const SKIP_SAMPLE_CAP: usize = 100;

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
    trigrams: BTreeSet<trigram::Trigram>,
    symbols: Vec<RawSymbol>,
    refs: Vec<RawRef>,
}

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
    /// File was skipped (binary or unreadable); carries the reason.
    Skipped(SkipReason),
}

fn process(entry: &WalkEntry, backend: &dyn IoBackend, config: &Config) -> Outcome {
    let data = match backend.read(&entry.path) {
        Ok(d) => d,
        // A read failure (permissions, vanished mid-walk) is a skip, not a hard
        // error — record it so it's visible rather than silently dropped.
        Err(_) => return Outcome::Skipped(SkipReason::ReadError),
    };
    if !config.index_binary && is_binary(&data) {
        return Outcome::Skipped(SkipReason::Binary);
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
    let (symbols, refs) = if lang.grammar().is_some() {
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
    }))
}

/// Run the read/parse stage over `candidates` in parallel, partitioning into
/// successfully processed files and skip records (binary/unreadable).
fn process_all(
    candidates: &[&WalkEntry],
    backend: &dyn IoBackend,
    config: &Config,
) -> (Vec<Processed>, Vec<Skipped>) {
    let outcomes: Vec<(String, Outcome)> = candidates
        .par_iter()
        .map(|e| (e.rel.clone(), process(e, backend, config)))
        .collect();
    let mut processed = Vec::with_capacity(outcomes.len());
    let mut skipped = Vec::new();
    for (rel, outcome) in outcomes {
        match outcome {
            Outcome::Indexed(p) => processed.push(*p),
            Outcome::Skipped(reason) => skipped.push(Skipped { rel, reason }),
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

        // Continue the segment-id counter from any existing manifest so the new
        // segment never collides with the old files we're about to replace.
        let mut meta = Meta::load(&self.paths.meta_file()).unwrap_or_default();
        let old_segments = std::mem::take(&mut meta.segments);

        let cache = Cache::open(&self.paths.cache_file())?;

        let walk::WalkResult {
            entries,
            skipped: walk_skips,
        } = walk::walk(self.paths, self.config)?;
        let candidates: Vec<&WalkEntry> = entries.iter().collect();
        let (processed, proc_skips) = process_all(&candidates, self.backend, self.config);

        let seg_id = meta.alloc_segment();
        let mut writer = SegmentWriter::new();
        let mut upserts = Vec::with_capacity(processed.len());
        for pf in &processed {
            let doc_id = writer.add_doc(
                pf.doc.clone(),
                &pf.trigrams,
                pf.symbols.clone(),
                pf.refs.clone(),
            );
            upserts.push((
                pf.rel.clone(),
                FileRecord {
                    inode: pf.inode,
                    mtime_ns: pf.mtime_ns,
                    size: pf.size,
                    hash: pf.hash,
                    segment_id: seg_id,
                    doc_id,
                    symbols: pf.symbols.len() as u32,
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

        for id in old_segments {
            self.remove_segment_files(id);
        }
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
        let cache = Cache::open(&self.paths.cache_file())?;
        let existing = cache.load_all()?;

        // Consistency guard: under normal operation every cache record points at
        // a segment listed in the manifest. If that invariant is broken (e.g. a
        // compaction that published the new manifest but was interrupted before
        // refreshing the cache), trusting the cache could tombstone the wrong
        // segment and leave duplicate or orphaned docs. The cache is rebuildable,
        // so degrade to a full rebuild instead.
        let live_segs: std::collections::HashSet<u64> = meta.segments.iter().copied().collect();
        if existing
            .values()
            .any(|r| !live_segs.contains(&r.segment_id))
        {
            // Release the cache handle before `index_full` reopens the database.
            drop(existing);
            drop(cache);
            return self.index_full();
        }

        let walk::WalkResult {
            entries,
            skipped: walk_skips,
        } = walk::walk(self.paths, self.config)?;
        let mut seen: HashMap<String, &WalkEntry> = HashMap::with_capacity(entries.len());
        for e in &entries {
            seen.insert(e.rel.clone(), e);
        }

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

        let (processed, proc_skips) = process_all(&candidates, self.backend, self.config);

        // Keep only entries whose content hash actually changed.
        let mut changed: Vec<Processed> = Vec::new();
        let mut touch_only: Vec<(String, FileRecord)> = Vec::new();
        for pf in processed {
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

        // Deleted files: in the cache but no longer on disk.
        let deleted: Vec<String> = existing
            .keys()
            .filter(|p| !seen.contains_key(*p))
            .cloned()
            .collect();

        // Tombstone old docs for changed and deleted files.
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
        for (seg_id, doc_ids) in &tombstones {
            self.tombstone(*seg_id, doc_ids)?;
        }

        // Write changed/new files into a fresh delta segment. Only allocate a
        // segment id when there is actually something to write, so no-op
        // incrementals don't burn ids.
        let mut upserts = touch_only;
        if !changed.is_empty() {
            let seg_id = meta.alloc_segment();
            let mut writer = SegmentWriter::new();
            for pf in &changed {
                let doc_id = writer.add_doc(
                    pf.doc.clone(),
                    &pf.trigrams,
                    pf.symbols.clone(),
                    pf.refs.clone(),
                );
                upserts.push((
                    pf.rel.clone(),
                    FileRecord {
                        inode: pf.inode,
                        mtime_ns: pf.mtime_ns,
                        size: pf.size,
                        hash: pf.hash,
                        segment_id: seg_id,
                        doc_id,
                        symbols: pf.symbols.len() as u32,
                    },
                ));
            }
            writer.write(self.paths, seg_id)?;
            meta.segments.push(seg_id);
        }

        cache.apply(&upserts, &deleted)?;

        let mut stats = IndexStats {
            files_indexed: changed.len(),
            files_removed: deleted.len(),
            symbols: changed.iter().map(|p| p.symbols.len()).sum(),
            segments: meta.segments.len(),
            ..Default::default()
        };
        stats.record_skips(walk_skips);
        stats.record_skips(proc_skips);

        // Maintain index-wide counts incrementally. Each live document maps to
        // exactly one cache record, so the deltas below keep `doc_count` /
        // `symbol_count` exact without re-opening and re-parsing every segment.
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
        meta.doc_count = doc_count.max(0) as u64;
        meta.symbol_count = sym_count.max(0) as u64;
        meta.record_git_head(&self.paths.root);
        meta.touch_now();
        meta.save(&self.paths.meta_file())?;

        // Auto-compact if we've accumulated too many segments.
        if meta.segments.len() > self.config.merge_threshold {
            self.compact()?;
        }
        Ok(stats)
    }

    /// Merge all live documents from every segment into a single compact
    /// segment. This reuses the already-indexed postings/symbols (no file reads,
    /// no re-parsing) and falls back to a full rebuild if anything goes wrong.
    pub fn compact(&self) -> Result<IndexStats> {
        match self.merge_segments() {
            Ok(stats) => Ok(stats),
            Err(e) => {
                tracing::warn!("merge compaction failed ({e}); falling back to full rebuild");
                self.index_full()
            }
        }
    }

    /// Core of [`compact`]: a true k-way merge over existing segments.
    fn merge_segments(&self) -> Result<IndexStats> {
        let mut meta = Meta::load(&self.paths.meta_file())?;
        if meta.segments.is_empty() {
            return Ok(IndexStats::default());
        }
        let cache = Cache::open(&self.paths.cache_file())?;
        let existing = cache.load_all()?;

        let segments: Vec<Segment> = meta
            .segments
            .iter()
            .map(|&id| Segment::open(self.paths, id))
            .collect::<Result<_>>()?;

        let mut docs: Vec<DocMeta> = Vec::new();
        let mut syms: Vec<SymbolEntry> = Vec::new();
        let mut refs: Vec<RefEntry> = Vec::new();
        let mut postings: BTreeMap<Trigram, RoaringBitmap> = BTreeMap::new();
        let mut upserts: Vec<(String, FileRecord)> = Vec::new();
        let new_seg_id = meta.alloc_segment();

        for seg in &segments {
            let mut remap: HashMap<u32, u32> = HashMap::new();
            for old_id in seg.all_live().iter() {
                let doc = match seg.doc(old_id) {
                    Some(d) => d,
                    None => continue,
                };
                let new_id = docs.len() as u32;
                remap.insert(old_id, new_id);
                let syms_before = syms.len();
                for s in seg.doc_syms(old_id) {
                    syms.push(SymbolEntry {
                        doc_id: new_id,
                        name: s.name.clone(),
                        kind: s.kind.clone(),
                        line_start: s.line_start,
                        line_end: s.line_end,
                        container: s.container.clone(),
                        signature: s.signature.clone(),
                    });
                }
                for r in seg.doc_refs(old_id) {
                    refs.push(RefEntry {
                        doc_id: new_id,
                        name: r.name.clone(),
                        kind: r.kind,
                        line: r.line,
                        column: r.column,
                    });
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
            seg.remap_postings(&remap, &mut postings)?;
        }

        let old_segments = std::mem::take(&mut meta.segments);
        let symbol_count = syms.len();
        let doc_count = docs.len();

        if docs.is_empty() {
            meta.segments = Vec::new();
        } else {
            write_segment_from_parts(self.paths, new_seg_id, &docs, &syms, &refs, postings)?;
            meta.segments = vec![new_seg_id];
        }

        // Publish the new manifest first so searches always see a consistent
        // index, then refresh the cache and reclaim the old segment files.
        meta.doc_count = doc_count as u64;
        meta.symbol_count = symbol_count as u64;
        meta.touch_now();
        meta.save(&self.paths.meta_file())?;

        cache.replace_all(&upserts)?;

        for id in old_segments {
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
