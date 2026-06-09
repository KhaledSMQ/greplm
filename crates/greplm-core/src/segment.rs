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
//! Everything except the live bitmap is immutable once written, so the loaded
//! tables and derived lookup maps live in an [`Arc<SegmentData>`] that a
//! reloading searcher can share instead of re-parsing (see [`Segment::reopen`]).

use std::collections::{BTreeMap, HashMap};
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
#[derive(Default)]
pub struct SegmentWriter {
    docs: Vec<DocMeta>,
    syms: Vec<SymbolEntry>,
    refs: Vec<RefEntry>,
    postings: BTreeMap<Trigram, RoaringBitmap>,
}

impl SegmentWriter {
    pub fn new() -> Self {
        Self::default()
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
        for t in trigrams {
            self.postings.entry(*t).or_default().insert(doc_id);
        }
        for s in symbols {
            self.syms.push(SymbolEntry {
                doc_id,
                name: s.name,
                kind: s.kind,
                line_start: s.line_start,
                line_end: s.line_end,
                container: s.container,
                signature: s.signature,
            });
        }
        for r in refs {
            self.refs.push(RefEntry {
                doc_id,
                name: r.name,
                kind: r.kind,
                line: r.line,
                column: r.column,
            });
        }
        doc_id
    }

    /// Serialize this segment to disk under the given segment id.
    pub fn write(self, paths: &Paths, seg_id: u64) -> Result<()> {
        std::fs::create_dir_all(paths.segments_dir())
            .map_err(|e| Error::io(paths.segments_dir(), e))?;
        let (post_blob, fst_entries) = build_postings_blob(self.postings)?;
        write_segment_files(
            paths,
            seg_id,
            &self.docs,
            &self.syms,
            &self.refs,
            &fst_entries,
            &post_blob,
        )
    }
}

/// Serialize the components of a segment to disk atomically. Shared by the
/// incremental writer and by compaction's merge path.
fn write_segment_files(
    paths: &Paths,
    seg_id: u64,
    docs: &[DocMeta],
    syms: &[SymbolEntry],
    refs: &[RefEntry],
    fst_entries: &[(Trigram, u64)],
    post_blob: &[u8],
) -> Result<()> {
    // FST keys must be inserted in lexicographic order; callers pass entries
    // sorted by trigram (BTreeMap iteration over [u8; 3] yields that order).
    let fst_path = paths.fst_file(seg_id);
    let mut fst_out = AtomicFile::create(&fst_path)?;
    let mut builder = fst::MapBuilder::new(BufWriter::new(fst_out.file()))?;
    for (tri, value) in fst_entries {
        builder.insert(tri, *value)?;
    }
    builder.finish()?;
    fst_out.commit()?;

    write_atomic(&paths.post_file(seg_id), post_blob)?;
    // Side tables use postcard (compact binary) rather than JSON: on large trees
    // these dominate on-disk size and cold-start parse time. The hot path (FST +
    // roaring + mmap) is unaffected.
    write_atomic(&paths.docs_file(seg_id), &postcard::to_allocvec(docs)?)?;
    write_atomic(&paths.syms_file(seg_id), &postcard::to_allocvec(syms)?)?;
    write_atomic(&paths.refs_file(seg_id), &postcard::to_allocvec(refs)?)?;

    // Initially every doc is live.
    let mut live = RoaringBitmap::new();
    live.insert_range(0..docs.len() as u32);
    write_bitmap(&paths.live_file(seg_id), &live)?;

    Ok(())
}

/// A serialized postings blob paired with the (trigram, packed value) entries
/// that index into it for the FST.
type PostingsBlob = (Vec<u8>, Vec<(Trigram, u64)>);

/// Build the postings blob and the (trigram, packed offset+cardinality) FST
/// entries from an in-memory posting map.
fn build_postings_blob(postings: BTreeMap<Trigram, RoaringBitmap>) -> Result<PostingsBlob> {
    let mut post_blob: Vec<u8> = Vec::new();
    let mut fst_entries: Vec<(Trigram, u64)> = Vec::with_capacity(postings.len());
    for (tri, mut bm) in postings.into_iter() {
        bm.optimize();
        let offset = post_blob.len() as u64;
        bm.serialize_into(&mut post_blob)
            .map_err(|e| Error::other(format!("roaring serialize: {e}")))?;
        fst_entries.push((tri, pack_entry(offset, bm.len())?));
    }
    Ok((post_blob, fst_entries))
}

/// Write a segment directly from prebuilt parts (used by compaction).
pub(crate) fn write_segment_from_parts(
    paths: &Paths,
    seg_id: u64,
    docs: &[DocMeta],
    syms: &[SymbolEntry],
    refs: &[RefEntry],
    postings: BTreeMap<Trigram, RoaringBitmap>,
) -> Result<()> {
    std::fs::create_dir_all(paths.segments_dir())
        .map_err(|e| Error::io(paths.segments_dir(), e))?;
    let (post_blob, fst_entries) = build_postings_blob(postings)?;
    write_segment_files(paths, seg_id, docs, syms, refs, &fst_entries, &post_blob)
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
    pub docs: Vec<DocMeta>,
    pub syms: Vec<SymbolEntry>,
    pub refs: Vec<RefEntry>,
    /// Symbol indices grouped by `doc_id` (a flattened CSR layout). Together with
    /// `sym_start` this gives O(1) access to a document's symbols instead of a
    /// full scan of `syms`.
    sym_order: Vec<u32>,
    /// Prefix offsets into `sym_order`; `sym_start[d]..sym_start[d + 1]` is the
    /// slice of symbol indices belonging to doc `d`. Length is `docs.len() + 1`.
    sym_start: Vec<u32>,
    /// Reference indices grouped by `doc_id` (CSR layout, mirrors `sym_order`).
    ref_order: Vec<u32>,
    /// Prefix offsets into `ref_order`; length is `docs.len() + 1`.
    ref_start: Vec<u32>,
    /// Lowercased symbol names, parallel to `syms`, precomputed once at open.
    sym_name_lower: Vec<String>,
    /// Symbol indices grouped by lowercased name. Turns name lookups
    /// (definitions, exact symbol queries) into O(results) instead of a scan
    /// of every symbol per query.
    sym_by_lower: HashMap<Box<str>, Vec<u32>>,
    /// Reference indices (calls *and* imports) grouped by referent name. Lets
    /// callers / references / blast-radius look up by name in O(results)
    /// instead of scanning every ref each query.
    ref_by_name: HashMap<Box<str>, Vec<u32>>,
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

        let docs_path = paths.docs_file(seg_id);
        let docs: Vec<DocMeta> = postcard::from_bytes(
            &std::fs::read(&docs_path).map_err(|e| Error::io(&docs_path, e))?,
        )?;

        let syms_path = paths.syms_file(seg_id);
        let syms: Vec<SymbolEntry> = postcard::from_bytes(
            &std::fs::read(&syms_path).map_err(|e| Error::io(&syms_path, e))?,
        )?;

        // Tolerate a missing refs file (treat as no refs) so a segment written
        // without any references can still be opened read-only.
        let refs_path = paths.refs_file(seg_id);
        let refs: Vec<RefEntry> = match std::fs::read(&refs_path) {
            Ok(bytes) => postcard::from_bytes(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(Error::io(&refs_path, e)),
        };

        let live = read_bitmap(&paths.live_file(seg_id))?;

        let (sym_order, sym_start) = build_doc_index(docs.len(), syms.iter().map(|s| s.doc_id));
        let (ref_order, ref_start) = build_doc_index(docs.len(), refs.iter().map(|r| r.doc_id));
        let sym_name_lower: Vec<String> =
            syms.iter().map(|s| s.name.to_ascii_lowercase()).collect();

        let mut sym_by_lower: HashMap<Box<str>, Vec<u32>> = HashMap::new();
        for (i, lower) in sym_name_lower.iter().enumerate() {
            sym_by_lower
                .entry(lower.as_str().into())
                .or_default()
                .push(i as u32);
        }

        let mut ref_by_name: HashMap<Box<str>, Vec<u32>> = HashMap::new();
        for (i, r) in refs.iter().enumerate() {
            ref_by_name
                .entry(r.name.as_str().into())
                .or_default()
                .push(i as u32);
        }

        Ok(Segment {
            id: seg_id,
            data: Arc::new(SegmentData {
                fst,
                post,
                docs,
                syms,
                refs,
                sym_order,
                sym_start,
                ref_order,
                ref_start,
                sym_name_lower,
                sym_by_lower,
                ref_by_name,
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

    /// The symbols defined in `doc_id`, in storage order. O(1) lookup.
    pub fn doc_syms(&self, doc_id: u32) -> impl Iterator<Item = &SymbolEntry> {
        let d = doc_id as usize;
        let (lo, hi) = if d + 1 < self.sym_start.len() {
            (self.sym_start[d] as usize, self.sym_start[d + 1] as usize)
        } else {
            (0, 0)
        };
        self.sym_order[lo..hi]
            .iter()
            .map(move |&i| &self.syms[i as usize])
    }

    /// The references (calls + imports) originating in `doc_id`. O(1) lookup.
    pub fn doc_refs(&self, doc_id: u32) -> impl Iterator<Item = &RefEntry> {
        let d = doc_id as usize;
        let (lo, hi) = if d + 1 < self.ref_start.len() {
            (self.ref_start[d] as usize, self.ref_start[d + 1] as usize)
        } else {
            (0, 0)
        };
        self.ref_order[lo..hi]
            .iter()
            .map(move |&i| &self.refs[i as usize])
    }

    /// References (calls and imports) whose name is exactly `name`, via the
    /// prebuilt name index (O(results), no full scan). Liveness is not filtered
    /// here; callers that care should check [`Segment::is_live`].
    pub fn refs_named(&self, name: &str) -> impl Iterator<Item = &RefEntry> {
        self.ref_by_name
            .get(name)
            .into_iter()
            .flatten()
            .map(move |&i| &self.refs[i as usize])
    }

    /// Call sites whose callee is exactly `name` (see [`Self::refs_named`]).
    pub fn calls_to(&self, name: &str) -> impl Iterator<Item = &RefEntry> {
        self.refs_named(name).filter(|r| r.kind == RefKind::Call)
    }

    /// Indices into `syms` whose lowercased name is exactly `lower`.
    /// O(results) via the prebuilt name index.
    pub fn syms_by_lower(&self, lower: &str) -> &[u32] {
        self.sym_by_lower
            .get(lower)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Lowercased name of the symbol at index `i` in `syms`.
    pub fn sym_name_lower(&self, i: usize) -> &str {
        &self.sym_name_lower[i]
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
        let slice = self.post.get(start..).ok_or_else(|| {
            Error::Corrupt(format!(
                "posting offset {start} out of range for postings blob of length {}",
                self.post.len()
            ))
        })?;
        RoaringBitmap::deserialize_from(slice)
            .map_err(|e| Error::Corrupt(format!("posting list: {e}")))
    }

    /// Add this segment's postings to `out`, remapping doc ids via `remap` and
    /// dropping any doc not present in the map (i.e. tombstoned/non-live).
    pub(crate) fn remap_postings(
        &self,
        remap: &HashMap<u32, u32>,
        out: &mut BTreeMap<Trigram, RoaringBitmap>,
    ) -> Result<()> {
        use fst::Streamer;
        let mut stream = self.fst.stream();
        while let Some((key, value)) = stream.next() {
            if key.len() != 3 {
                continue;
            }
            let tri: Trigram = [key[0], key[1], key[2]];
            let bm = self.posting_at(unpack_offset(value))?;
            let dest = out.entry(tri).or_default();
            for old in bm.iter() {
                if let Some(&new_id) = remap.get(&old) {
                    dest.insert(new_id);
                }
            }
        }
        Ok(())
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

/// Build a CSR-style index grouping row indices by their `doc_id`. Shared by
/// the symbol and reference per-document lookups.
fn build_doc_index(n: usize, doc_ids: impl Iterator<Item = u32> + Clone) -> (Vec<u32>, Vec<u32>) {
    let mut counts = vec![0u32; n + 1];
    let mut total = 0usize;
    for d in doc_ids.clone() {
        let d = d as usize;
        if d < n {
            counts[d] += 1;
            total += 1;
        }
    }
    // Prefix-sum into start offsets.
    let mut start = vec![0u32; n + 1];
    let mut acc = 0u32;
    for d in 0..n {
        start[d] = acc;
        acc += counts[d];
    }
    start[n] = acc;
    // Scatter row indices into their doc's slot.
    let mut order = vec![0u32; total];
    let mut cursor: Vec<u32> = start[..n].to_vec();
    for (i, d) in doc_ids.enumerate() {
        let d = d as usize;
        if d < n {
            order[cursor[d] as usize] = i as u32;
            cursor[d] += 1;
        }
    }
    (order, start)
}

pub(crate) fn write_bitmap(path: &std::path::Path, bm: &RoaringBitmap) -> Result<()> {
    let mut buf = Vec::with_capacity(bm.serialized_size());
    bm.serialize_into(&mut buf)
        .map_err(|e| Error::other(format!("roaring serialize: {e}")))?;
    write_atomic(path, &buf)
}

pub(crate) fn read_bitmap(path: &std::path::Path) -> Result<RoaringBitmap> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    RoaringBitmap::deserialize_from(&bytes[..])
        .map_err(|e| Error::Corrupt(format!("live bitmap: {e}")))
}
