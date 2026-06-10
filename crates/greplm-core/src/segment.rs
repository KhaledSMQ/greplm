//! On-disk index segments.
//!
//! Each segment is a set of files:
//!   * `seg-N.fst`  - an FST mapping each trigram (3 bytes) to a packed value
//!     holding the posting-list byte offset *and* its cardinality (see
//!     [`pack_entry`]), so query planning can pick the rarest trigrams without
//!     touching the postings blob
//!   * `seg-N.post` - concatenated roaring bitmaps (posting lists) at those offsets
//!   * `seg-N.docs` - postcard-encoded `Vec<`[`DocMeta`]`>` (one per document)
//!   * `seg-N.syms` - postcard-encoded `Vec<`[`SymbolEntry`]`>`
//!   * `seg-N.refs` - postcard-encoded `Vec<`[`RefEntry`]`>` (call sites + imports)
//!   * `seg-N.live` - a roaring bitmap of live (non-tombstoned) doc IDs
//!
//! The FST and postings blob are mmap'd for zero-copy, page-cache-backed reads.
//! Doc and symbol tables are small relative to content and loaded into memory.
//!
//! Every segment file carries an 8-byte xxh3 checksum footer, verified at open
//! so silent corruption surfaces as [`Error::Corrupt`] (triggering the
//! self-healing rebuild) instead of garbage results or a panic. The FST
//! additionally has its own internal checksum.
//!
//! Everything except the live bitmap is immutable once written, so the loaded
//! tables and derived lookup maps live in an [`Arc<SegmentData>`] that a
//! reloading searcher can share instead of re-parsing (see [`Segment::reopen`]).

use std::io::BufWriter;
use std::ops::Deref;
use std::sync::Arc;

use memmap2::Mmap;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fsutil::{write_atomic, AtomicFile};
use crate::paths::Paths;
use crate::trigram::{self, Trigram, TrigramDnf, TrigramQuery};

/// Metadata for one indexed document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocMeta {
    /// Path relative to the project root.
    pub path: String,
    /// Language id (see [`crate::lang::Language::id`]).
    pub lang: String,
    pub size: u64,
    /// Fast content hash at index time (xxh3).
    pub hash: u64,
    pub lines: u32,
}

/// A symbol definition extracted from a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolEntry {
    pub doc_id: u32,
    pub name: String,
    /// Kind, e.g. "function", "class", "struct".
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    /// Enclosing named container (e.g. the class a method belongs to).
    ///
    /// No `skip_serializing_if`: the side tables use postcard, a
    /// non-self-describing format where every field must be encoded
    /// unconditionally or the byte stream desyncs from the reader's schema.
    pub container: Option<String>,
    /// Compact one-line signature.
    pub signature: Option<String>,
}

/// A symbol before a document id is assigned.
#[derive(Debug, Clone)]
pub struct RawSymbol {
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    pub container: Option<String>,
    pub signature: Option<String>,
}

/// The kind of a structural reference. Stored as a 1-byte enum (rather than a
/// heap `String`) since refs are the most numerous index records; serializes to
/// the same `"call"`/`"import"` tokens on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    Call,
    Import,
}

impl RefKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RefKind::Call => "call",
            RefKind::Import => "import",
        }
    }
}

/// A structural reference (call site or import) extracted from a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefEntry {
    pub doc_id: u32,
    pub name: String,
    pub kind: RefKind,
    pub line: u32,
    pub column: u32,
}

/// A reference before a document id is assigned.
#[derive(Debug, Clone)]
pub struct RawRef {
    pub name: String,
    pub kind: RefKind,
    pub line: u32,
    pub column: u32,
}

// ---------------------------------------------------------------------------
// FST value packing
// ---------------------------------------------------------------------------

/// Bits of the packed FST value reserved for the posting-list byte offset.
/// 40 bits addresses a 1 TiB postings blob, far beyond any real segment.
const OFFSET_BITS: u32 = 40;
const OFFSET_MASK: u64 = (1 << OFFSET_BITS) - 1;
/// The cardinality saturates at 2^24-1; beyond that the exact count no longer
/// affects rarest-first ordering meaningfully.
const CARD_CAP: u64 = (1 << (64 - OFFSET_BITS)) - 1;

/// Maximum trigrams intersected per AND-group, rarest first. Each additional
/// intersection costs a full posting-list deserialize for rapidly diminishing
/// selectivity, so long literals only pay for their most selective trigrams;
/// the exact matcher verifies whatever the looser filter lets through.
const MAX_GROUP_TRIGRAMS: usize = 4;

/// Pack a posting-list offset and its cardinality into one FST value.
fn pack_entry(offset: u64, cardinality: u64) -> Result<u64> {
    if offset > OFFSET_MASK {
        return Err(Error::other(format!(
            "postings blob offset {offset} exceeds the packable maximum"
        )));
    }
    Ok((cardinality.min(CARD_CAP) << OFFSET_BITS) | offset)
}

fn unpack_offset(value: u64) -> u64 {
    value & OFFSET_MASK
}

fn unpack_card(value: u64) -> u64 {
    value >> OFFSET_BITS
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Accumulates documents and builds a segment on disk.
///
/// Symbols and refs stream straight into the columnar table builders as docs
/// are added — the writer never materializes per-row structs, so peak memory
/// during a build is the packed table bytes plus the postings pairs.
pub struct SegmentWriter {
    docs: Vec<DocMeta>,
    syms: crate::table::SymTableBuilder,
    refs: crate::table::RefTableBuilder,
    /// Flat postings pairs, `(trigram key << 32) | doc_id`, inverted by one
    /// parallel sort at write time (Lucene-style sort-based inversion) instead
    /// of millions of cache-hostile tree probes during the build.
    pairs: Vec<u64>,
}

impl Default for SegmentWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl SegmentWriter {
    pub fn new() -> Self {
        SegmentWriter {
            docs: Vec::new(),
            syms: crate::table::SymTableBuilder::new(),
            refs: crate::table::RefTableBuilder::new(),
            pairs: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    pub fn doc_count(&self) -> usize {
        self.docs.len()
    }

    pub fn symbol_count(&self) -> usize {
        self.syms.len()
    }

    /// Add a document and return its assigned doc id. `trigrams` must be the
    /// document's distinct trigrams (any order; typically sorted from
    /// [`trigram::extract`]).
    pub fn add_doc(
        &mut self,
        meta: DocMeta,
        trigrams: &[Trigram],
        symbols: Vec<RawSymbol>,
        refs: Vec<RawRef>,
    ) -> u32 {
        let doc_id = self.docs.len() as u32;
        self.docs.push(meta);
        self.pairs.extend(
            trigrams
                .iter()
                .map(|t| (u64::from(trigram::key_of(t)) << 32) | u64::from(doc_id)),
        );
        for s in &symbols {
            self.syms
                .push(
                    doc_id,
                    &s.name,
                    &s.kind,
                    s.line_start,
                    s.line_end,
                    s.container.as_deref(),
                    s.signature.as_deref(),
                )
                .expect("writer doc ids are ascending");
        }
        for r in &refs {
            self.refs
                .push(doc_id, &r.name, r.kind, r.line, r.column)
                .expect("writer doc ids are ascending");
        }
        doc_id
    }

    /// Serialize this segment to disk under the given segment id.
    pub fn write(self, paths: &Paths, seg_id: u64) -> Result<()> {
        let SegmentWriter {
            docs,
            syms,
            refs,
            pairs,
        } = self;
        // The postings inversion and the two table finishes (each ending in a
        // parallel name sort) are independent; overlap them.
        let (postings, tables) = rayon::join(
            || build_postings_blob(pairs),
            || rayon::join(|| syms.finish(docs.len()), || refs.finish(docs.len())),
        );
        let (post_blob, fst_entries) = postings?;
        let (syms_enc, refs_enc) = tables;
        write_segment_files(
            paths,
            seg_id,
            &docs,
            syms_enc?,
            refs_enc?,
            &fst_entries,
            post_blob,
        )
    }
}

/// Append the 8-byte xxh3 checksum footer carried by every segment file.
fn append_checksum(buf: &mut Vec<u8>) {
    let h = xxhash_rust::xxh3::xxh3_64(buf);
    buf.extend_from_slice(&h.to_le_bytes());
}

/// Verify a checksum footer and return the payload it covers.
fn verify_checksum<'a>(bytes: &'a [u8], what: &str) -> Result<&'a [u8]> {
    if bytes.len() < 8 {
        return Err(Error::Corrupt(format!(
            "{what}: too short for checksum footer"
        )));
    }
    let (payload, footer) = bytes.split_at(bytes.len() - 8);
    let want = u64::from_le_bytes(footer.try_into().expect("8-byte footer"));
    if xxhash_rust::xxh3::xxh3_64(payload) != want {
        return Err(Error::Corrupt(format!("{what}: checksum mismatch")));
    }
    Ok(payload)
}

/// Serialize and checksum a postcard side table.
fn encode_table<T: Serialize>(rows: &[T]) -> Result<Vec<u8>> {
    let mut buf = postcard::to_allocvec(rows)?;
    append_checksum(&mut buf);
    Ok(buf)
}

/// Read and decode a checksummed postcard side table.
fn read_table<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    what: &str,
) -> Result<Vec<T>> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    Ok(postcard::from_bytes(verify_checksum(&bytes, what)?)?)
}

/// Serialize the components of a segment to disk atomically. Shared by the
/// incremental writer and by compaction's merge path. Takes the postings blob
/// by value to append its checksum footer without copying; the side tables
/// arrive pre-encoded and are streamed to disk section by section.
pub(crate) fn write_segment_files(
    paths: &Paths,
    seg_id: u64,
    docs: &[DocMeta],
    syms: crate::table::EncodedTable,
    refs: crate::table::EncodedTable,
    fst_entries: &[(Trigram, u64)],
    mut post_blob: Vec<u8>,
) -> Result<()> {
    std::fs::create_dir_all(paths.segments_dir())
        .map_err(|e| Error::io(paths.segments_dir(), e))?;
    // FST keys must be inserted in lexicographic order; callers pass entries
    // sorted by trigram (sort-based inversion and the k-way merge both yield
    // that order). The FST carries its own internal checksum.
    let fst_path = paths.fst_file(seg_id);
    let mut fst_out = AtomicFile::create(&fst_path)?;
    let mut builder = fst::MapBuilder::new(BufWriter::new(fst_out.file()))?;
    for (tri, value) in fst_entries {
        builder.insert(tri, *value)?;
    }
    builder.finish()?;
    fst_out.commit()?;

    append_checksum(&mut post_blob);
    write_atomic(&paths.post_file(seg_id), &post_blob)?;
    // The doc table is small (one row per file) and eagerly decoded at open.
    write_atomic(&paths.docs_file(seg_id), &encode_table(docs)?)?;
    syms.write_atomic(&paths.syms_file(seg_id))?;
    refs.write_atomic(&paths.refs_file(seg_id))?;

    // Initially every doc is live.
    let mut live = RoaringBitmap::new();
    live.insert_range(0..docs.len() as u32);
    write_bitmap(&paths.live_file(seg_id), &live)?;

    Ok(())
}

/// A serialized postings blob paired with the (trigram, packed value) entries
/// that index into it for the FST.
pub(crate) type PostingsBlob = (Vec<u8>, Vec<(Trigram, u64)>);

/// Build the postings blob and the (trigram, packed offset+cardinality) FST
/// entries from flat `(trigram key << 32) | doc_id` pairs: one parallel sort,
/// then a parallel serialization pass over chunks split at trigram-run
/// boundaries (each chunk serializes whole runs into a local buffer with
/// local offsets; the chunks are then concatenated with one offset fix-up).
fn build_postings_blob(mut pairs: Vec<u64>) -> Result<PostingsBlob> {
    use rayon::prelude::*;
    pairs.par_sort_unstable();
    if pairs.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    // Chunk boundaries, advanced to the next run boundary so no trigram's
    // postings straddle two chunks.
    let n = pairs.len();
    let parts = rayon::current_num_threads().clamp(1, 64);
    let mut bounds: Vec<usize> = vec![0];
    for p in 1..parts {
        // `max(1)` keeps the look-behind in bounds when n < parts.
        let mut at = (n * p / parts).max(1);
        while at < n && (pairs[at - 1] >> 32) == (pairs[at] >> 32) {
            at += 1;
        }
        if at > *bounds.last().expect("non-empty") && at < n {
            bounds.push(at);
        }
    }
    bounds.push(n);

    // Per chunk: a local blob plus (key, local offset, cardinality) entries.
    type Chunk = (Vec<u8>, Vec<(u32, u64, u64)>);
    let chunks: Vec<Chunk> = bounds
        .par_windows(2)
        .map(|w| {
            let span = &pairs[w[0]..w[1]];
            let mut blob: Vec<u8> = Vec::new();
            let mut entries: Vec<(u32, u64, u64)> = Vec::new();
            let mut i = 0usize;
            while i < span.len() {
                let key = (span[i] >> 32) as u32;
                let start = i;
                while i < span.len() && (span[i] >> 32) as u32 == key {
                    i += 1;
                }
                // Within a run, doc ids are strictly ascending: the run is a
                // sorted u64 range sharing its high 32 bits, and each doc
                // contributes a trigram at most once (extract() deduplicates).
                let mut bm =
                    RoaringBitmap::from_sorted_iter(span[start..i].iter().map(|&p| p as u32))
                        .map_err(|e| Error::other(format!("postings pairs not sorted: {e}")))?;
                bm.optimize();
                let offset = blob.len() as u64;
                bm.serialize_into(&mut blob)
                    .map_err(|e| Error::other(format!("roaring serialize: {e}")))?;
                entries.push((key, offset, bm.len()));
            }
            Ok((blob, entries))
        })
        .collect::<Result<_>>()?;
    drop(pairs);

    // Stitch: concatenate blobs and rebase each chunk's offsets.
    let total: usize = chunks.iter().map(|(b, _)| b.len()).sum();
    let mut post_blob: Vec<u8> = Vec::with_capacity(total);
    let mut fst_entries: Vec<(Trigram, u64)> =
        Vec::with_capacity(chunks.iter().map(|(_, e)| e.len()).sum());
    for (blob, entries) in chunks {
        let base = post_blob.len() as u64;
        post_blob.extend_from_slice(&blob);
        for (key, offset, card) in entries {
            fst_entries.push((trigram::tri_of(key), pack_entry(base + offset, card)?));
        }
    }
    Ok((post_blob, fst_entries))
}

/// Stream-merge the postings of several segments into one blob, remapping doc
/// ids via `remaps` (one table per segment, indexed by old doc id; `u32::MAX`
/// marks a dropped/tombstoned doc).
///
/// A k-way union over the segments' FST term dictionaries visits trigrams in
/// lexicographic order, so memory stays at O(segments) plus a single output
/// posting list — the merged index's postings are never materialized at once.
pub(crate) fn merge_postings(segments: &[Segment], remaps: &[Vec<u32>]) -> Result<PostingsBlob> {
    use fst::Streamer;
    let mut op = fst::map::OpBuilder::new();
    for seg in segments {
        op.push(seg.data.fst.stream());
    }
    let mut union = op.union();
    let mut post_blob: Vec<u8> = Vec::new();
    let mut fst_entries: Vec<(Trigram, u64)> = Vec::new();
    while let Some((key, vals)) = union.next() {
        if key.len() != 3 {
            continue;
        }
        let tri: Trigram = [key[0], key[1], key[2]];
        let mut out = RoaringBitmap::new();
        for iv in vals {
            let seg = &segments[iv.index];
            let remap = &remaps[iv.index];
            let bm = seg.data.posting_at(unpack_offset(iv.value))?;
            for old in bm {
                if let Some(&new_id) = remap.get(old as usize) {
                    if new_id != u32::MAX {
                        out.insert(new_id);
                    }
                }
            }
        }
        if out.is_empty() {
            continue;
        }
        out.optimize();
        let offset = post_blob.len() as u64;
        out.serialize_into(&mut post_blob)
            .map_err(|e| Error::other(format!("roaring serialize: {e}")))?;
        fst_entries.push((tri, pack_entry(offset, out.len())?));
    }
    Ok((post_blob, fst_entries))
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// The immutable, shareable portion of an opened segment: mmaps, decoded side
/// tables, and the derived lookup structures. Wrapped in an `Arc` so a daemon
/// reloading its searcher after an incremental index can reuse unchanged
/// segments instead of re-parsing and re-deriving everything.
pub struct SegmentData {
    fst: fst::Map<Mmap>,
    post: Mmap,
    /// Logical length of the postings blob (the mmap minus its checksum
    /// footer); posting offsets must never slice past this.
    post_len: usize,
    pub docs: Vec<DocMeta>,
    /// Columnar mmap-backed symbol table: rows decode on demand, name lookups
    /// go through a persisted FST, and the fuzzy-scan path walks packed name
    /// columns — nothing is materialized at open.
    syms: crate::table::SymTable,
    /// Columnar reference table; `None` when the segment has no refs file.
    refs: Option<crate::table::RefTable>,
}

/// A read-only, mmap-backed view of a segment: shared immutable data plus this
/// open's snapshot of the live bitmap (the only part that changes on disk).
pub struct Segment {
    pub id: u64,
    data: Arc<SegmentData>,
    live: RoaringBitmap,
}

impl Deref for Segment {
    type Target = SegmentData;
    fn deref(&self) -> &SegmentData {
        &self.data
    }
}

impl Segment {
    pub fn open(paths: &Paths, seg_id: u64) -> Result<Segment> {
        let fst_path = paths.fst_file(seg_id);
        let fst_file = std::fs::File::open(&fst_path).map_err(|e| Error::io(&fst_path, e))?;
        let fst_mmap = unsafe { Mmap::map(&fst_file).map_err(|e| Error::io(&fst_path, e))? };
        let fst = fst::Map::new(fst_mmap)?;
        // `Map::new` only validates the FST header/length, not the node graph.
        // A corrupt or truncated `.fst` whose header survives would otherwise
        // panic with an out-of-bounds index deep in the fst crate's traversal
        // (`Node::new`) on the first query. Verify the stored checksum up front
        // so corruption surfaces as `Corrupt` (triggering the self-healing
        // rebuild) instead of an abort.
        fst.as_fst()
            .verify()
            .map_err(|e| Error::Corrupt(format!("fst checksum: {e}")))?;

        let post_path = paths.post_file(seg_id);
        let post_file = std::fs::File::open(&post_path).map_err(|e| Error::io(&post_path, e))?;
        let post = unsafe { Mmap::map(&post_file).map_err(|e| Error::io(&post_path, e))? };
        // Verify the blob's checksum footer up front (one sequential pass, same
        // policy as the FST verification above) so a flipped bit surfaces as
        // `Corrupt` at open instead of a wrong or undecodable posting later.
        let post_len = verify_checksum(&post, "postings blob")?.len();

        let docs: Vec<DocMeta> = read_table(&paths.docs_file(seg_id), "docs table")?;
        let syms = crate::table::SymTable::open(&paths.syms_file(seg_id))?;

        // Tolerate a missing refs file (treat as no refs) so a segment written
        // without any references can still be opened read-only.
        let refs_path = paths.refs_file(seg_id);
        let refs = match crate::table::RefTable::open(&refs_path) {
            Ok(t) => Some(t),
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };

        // The side tables and the docs table are separate files; make sure
        // they belong to the same generation, otherwise per-doc CSR lookups
        // would silently return the wrong rows.
        if syms.doc_count() != docs.len()
            || refs.as_ref().is_some_and(|r| r.doc_count() != docs.len())
        {
            return Err(Error::Corrupt(format!(
                "segment {seg_id}: side-table doc count does not match docs table"
            )));
        }

        let live = read_bitmap(&paths.live_file(seg_id))?;

        Ok(Segment {
            id: seg_id,
            data: Arc::new(SegmentData {
                fst,
                post,
                post_len,
                docs,
                syms,
                refs,
            }),
            live,
        })
    }

    /// Re-open this segment cheaply: share the immutable data and reload only
    /// the live bitmap (the single mutable file). Sound because segment ids are
    /// never reused — the same id always names the same immutable content.
    pub fn reopen(&self, paths: &Paths) -> Result<Segment> {
        let live = read_bitmap(&paths.live_file(self.id))?;
        Ok(Segment {
            id: self.id,
            data: self.data.clone(),
            live,
        })
    }

    pub fn is_live(&self, doc_id: u32) -> bool {
        self.live.contains(doc_id)
    }

    /// Subtract pending tombstones from this open's live snapshot. Readers use
    /// this to honor deletes that are already published in the manifest but
    /// not yet applied to the on-disk live bitmap (see
    /// [`crate::meta::Meta::pending_tombstones`]).
    pub fn subtract_live(&mut self, doc_ids: &[u32]) {
        for &id in doc_ids {
            self.live.remove(id);
        }
    }

    pub fn live_count(&self) -> u64 {
        self.live.len()
    }

    /// All live doc ids in this segment.
    pub fn all_live(&self) -> RoaringBitmap {
        self.live.clone()
    }

    /// Compute candidate doc ids satisfying the trigram query, intersected with
    /// the live set. An unconstrained query yields all live docs.
    pub fn candidates(&self, query: &TrigramQuery) -> Result<RoaringBitmap> {
        let mut filtering = query
            .dnfs
            .iter()
            .filter(|d| trigram::dnf_filters(d))
            .peekable();
        if filtering.peek().is_none() {
            return Ok(self.all_live());
        }
        let mut result: Option<RoaringBitmap> = None;
        for dnf in filtering {
            let bm = self.data.dnf_bitmap(dnf)?;
            result = Some(match result.take() {
                None => bm,
                Some(a) => a & bm,
            });
            if result.as_ref().is_some_and(|b| b.is_empty()) {
                break;
            }
        }
        let mut out = result.unwrap_or_default();
        out &= &self.live;
        Ok(out)
    }
}

impl SegmentData {
    pub fn doc(&self, doc_id: u32) -> Option<&DocMeta> {
        self.docs.get(doc_id as usize)
    }

    /// Number of symbol rows in this segment (live or not).
    pub fn sym_count(&self) -> usize {
        self.syms.len()
    }

    /// Decode symbol row `i`. `None` for out-of-range ids or a corrupt row.
    pub fn sym(&self, i: u32) -> Option<SymbolEntry> {
        self.syms.get(i)
    }

    /// `(row id, name, lowercased name)` of every symbol — the fuzzy-scan
    /// path. Walks the packed name columns only; rows are decoded lazily by
    /// the caller for actual matches.
    pub fn sym_names(&self) -> impl Iterator<Item = (u32, &str, &str)> {
        self.syms.names()
    }

    /// The symbols defined in `doc_id`, in storage order. O(1) range lookup.
    pub fn doc_syms(&self, doc_id: u32) -> impl Iterator<Item = SymbolEntry> + '_ {
        self.syms
            .doc_range(doc_id)
            .filter_map(move |i| self.syms.get(i))
    }

    /// Number of symbols defined in `doc_id`, without decoding any row.
    pub fn doc_sym_count(&self, doc_id: u32) -> u32 {
        let r = self.syms.doc_range(doc_id);
        r.end - r.start
    }

    /// The references (calls + imports) originating in `doc_id`.
    pub fn doc_refs(&self, doc_id: u32) -> impl Iterator<Item = RefEntry> + '_ {
        self.refs
            .iter()
            .flat_map(move |t| t.doc_range(doc_id).filter_map(move |i| t.get(i)))
    }

    /// References (calls and imports) whose name is exactly `name`, via the
    /// persisted name index (O(results), no full scan). Liveness is not
    /// filtered here; callers that care should check [`Segment::is_live`].
    pub fn refs_named<'s>(&'s self, name: &'s str) -> impl Iterator<Item = RefEntry> + 's {
        self.refs
            .iter()
            .flat_map(move |t| t.rows_named(name).filter_map(move |i| t.get(i)))
    }

    /// Call sites whose callee is exactly `name` (see [`Self::refs_named`]).
    pub fn calls_to<'s>(&'s self, name: &'s str) -> impl Iterator<Item = RefEntry> + 's {
        self.refs_named(name).filter(|r| r.kind == RefKind::Call)
    }

    /// Symbol row ids whose lowercased name is exactly `lower`. O(results)
    /// via the persisted name FST.
    pub fn syms_by_lower<'s>(&'s self, lower: &'s str) -> impl Iterator<Item = u32> + 's {
        self.syms.rows_named(lower)
    }

    /// Name of symbol row `i` (borrowed from the name column).
    pub fn sym_name(&self, i: u32) -> &str {
        self.syms.name(i)
    }

    /// Lowercased name of symbol row `i` (borrowed from the name column).
    pub fn sym_name_lower(&self, i: u32) -> &str {
        self.syms.name_lower(i)
    }

    /// The packed FST entry for a trigram: posting offset + cardinality.
    fn posting_entry(&self, tri: Trigram) -> Option<u64> {
        self.fst.get(tri)
    }

    /// Deserialize the posting list stored at `offset` in the postings blob.
    ///
    /// `offset` comes from the FST term dictionary, which on a corrupt or
    /// truncated index can point past the end of the mmap'd blob. Bounds-check
    /// it so a bad offset is reported as `Corrupt` (letting the self-healing
    /// path rebuild) instead of panicking with an out-of-range slice index.
    fn posting_at(&self, offset: u64) -> Result<RoaringBitmap> {
        let start = offset as usize;
        let slice = self.post.get(start..self.post_len).ok_or_else(|| {
            Error::Corrupt(format!(
                "posting offset {start} out of range for postings blob of length {}",
                self.post_len
            ))
        })?;
        RoaringBitmap::deserialize_from(slice)
            .map_err(|e| Error::Corrupt(format!("posting list: {e}")))
    }

    /// OR together the AND-groups of one DNF.
    fn dnf_bitmap(&self, dnf: &TrigramDnf) -> Result<RoaringBitmap> {
        let mut acc = RoaringBitmap::new();
        for group in dnf {
            acc |= self.group_bitmap(group)?;
        }
        Ok(acc)
    }

    /// AND together a group of trigrams, deserializing only the
    /// [`MAX_GROUP_TRIGRAMS`] rarest posting lists. The cardinality packed in
    /// the FST value orders the trigrams *before* any posting list is touched,
    /// and a trigram absent from the index empties the group immediately.
    /// Skipping the commoner trigrams only widens the candidate set, never
    /// narrows it, so this is sound.
    fn group_bitmap(&self, group: &[Trigram]) -> Result<RoaringBitmap> {
        let mut entries: Vec<u64> = Vec::with_capacity(group.len());
        for tri in group {
            match self.posting_entry(*tri) {
                Some(v) => entries.push(v),
                // No document contains this trigram => the AND is empty.
                None => return Ok(RoaringBitmap::new()),
            }
        }
        entries.sort_unstable_by_key(|&v| unpack_card(v));
        entries.truncate(MAX_GROUP_TRIGRAMS);

        let mut acc: Option<RoaringBitmap> = None;
        for v in entries {
            let bm = self.posting_at(unpack_offset(v))?;
            acc = Some(match acc.take() {
                None => bm,
                Some(a) => a & bm,
            });
            if acc.as_ref().is_some_and(|b| b.is_empty()) {
                break;
            }
        }
        Ok(acc.unwrap_or_default())
    }
}

pub(crate) fn write_bitmap(path: &std::path::Path, bm: &RoaringBitmap) -> Result<()> {
    let mut buf = Vec::with_capacity(bm.serialized_size() + 8);
    bm.serialize_into(&mut buf)
        .map_err(|e| Error::other(format!("roaring serialize: {e}")))?;
    append_checksum(&mut buf);
    write_atomic(path, &buf)
}

pub(crate) fn read_bitmap(path: &std::path::Path) -> Result<RoaringBitmap> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    RoaringBitmap::deserialize_from(verify_checksum(&bytes, "live bitmap")?)
        .map_err(|e| Error::Corrupt(format!("live bitmap: {e}")))
}
