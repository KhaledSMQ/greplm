//! Columnar, mmap-backed side tables for symbols and references.
//!
//! Up to format v5 these tables were one postcard `Vec<T>` per file: opening a
//! segment forced a full decode (hundreds of MB of heap `String`s on large
//! trees) plus an eager build of the per-name lookup maps — ~2 s and ~1.8 GB
//! on a Linux-kernel-sized index. The columnar layout makes open effectively
//! O(1): rows are decoded on demand straight out of the mmap, names live in
//! packed string columns scanned without decoding rows, and the name→rows
//! lookup is a persisted FST built at write time.
//!
//! File layout (all integers little-endian):
//!
//! ```text
//! magic [4] | row_count u32 | doc_count u32
//! section table: N × (offset u64, len u64, xxh3 u64)
//! section bytes ...
//! ```
//!
//! Symbol sections: row offsets (u64×rows+1), row data (postcard per row),
//! doc CSR (u32×docs+1), name column, lowercased-name column, name FST
//! (lowercased name → packed (start, count) into the ids section), name→row
//! ids (u32), kind table (interned kind strings).
//!
//! Ref sections: row offsets, row data, doc CSR, name column, name FST
//! (exact name), name→row ids.
//!
//! Every section except the row data is checksum-verified at open. Those
//! sections are small (offsets, CSR, FST), so open stays cheap, while
//! everything that could cause a panic if corrupt (the FST graph, the offset
//! tables that other slicing trusts) is proven intact up front. Row data is
//! bounds-checked and decoded per row, where corruption surfaces as a skipped
//! row (logged) rather than a crash — and the offsets that frame it are
//! verified, so a bad row cannot leak into a neighbor.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::segment::{RefEntry, RefKind, SymbolEntry};

const SYM_MAGIC: [u8; 4] = *b"GLS1";
const REF_MAGIC: [u8; 4] = *b"GLR1";

/// Section indices shared by both table kinds.
const SEC_ROW_OFFSETS: usize = 0;
/// The one section that is *not* eagerly verified (it dominates file size).
const SEC_ROWS: usize = 1;
const SEC_DOC_STARTS: usize = 2;
const SEC_NAME_OFFSETS: usize = 3;
const SEC_NAME_BYTES: usize = 4;
// Symbol-only sections.
const SYM_SEC_LOWER_OFFSETS: usize = 5;
const SYM_SEC_LOWER_BYTES: usize = 6;
const SYM_SEC_FST: usize = 7;
const SYM_SEC_FST_ROWS: usize = 8;
const SYM_SEC_KINDS: usize = 9;
const SYM_SECTIONS: usize = 10;
// Ref-only sections.
const REF_SEC_FST: usize = 5;
const REF_SEC_FST_ROWS: usize = 6;
const REF_SECTIONS: usize = 7;

/// On-disk row payload for a symbol. The name lives in the name columns and
/// the kind in the interned kind table, so the hot scan paths never decode
/// rows and the row blob stays small.
#[derive(Deserialize)]
struct SymRow {
    doc_id: u32,
    kind_id: u32,
    line_start: u32,
    line_end: u32,
    container: Option<String>,
    signature: Option<String>,
}

/// Borrowed twin of [`SymRow`] for serialization: lets the builders encode
/// straight from `&str` fields without owning them. Field order and types
/// must match `SymRow` exactly (postcard encodes `&str` and `String`
/// identically).
#[derive(Serialize)]
struct SymRowRef<'a> {
    doc_id: u32,
    kind_id: u32,
    line_start: u32,
    line_end: u32,
    container: Option<&'a str>,
    signature: Option<&'a str>,
}

/// On-disk row payload for a reference; the name lives in the name column.
#[derive(Serialize, Deserialize)]
struct RefRow {
    doc_id: u32,
    kind: RefKind,
    line: u32,
    column: u32,
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// An encoded table, held as discrete sections until written. Kept apart so
/// the file can be streamed to disk section by section instead of
/// concatenated into one transient buffer the size of the whole file.
pub(crate) struct EncodedTable {
    magic: [u8; 4],
    row_count: u32,
    doc_count: u32,
    sections: Vec<Vec<u8>>,
}

impl EncodedTable {
    /// Header + checksummed section table (section bytes follow on disk).
    fn header(&self) -> Vec<u8> {
        let data_at = 12 + self.sections.len() * 24;
        let mut out = Vec::with_capacity(data_at);
        out.extend_from_slice(&self.magic);
        out.extend_from_slice(&self.row_count.to_le_bytes());
        out.extend_from_slice(&self.doc_count.to_le_bytes());
        let mut offset = data_at as u64;
        for s in &self.sections {
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&(s.len() as u64).to_le_bytes());
            out.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(s).to_le_bytes());
            offset += s.len() as u64;
        }
        out
    }

    /// Stream the table to `path` atomically (temp + fsync + rename).
    pub fn write_atomic(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        let mut af = crate::fsutil::AtomicFile::create(path)?;
        {
            let mut w = std::io::BufWriter::new(af.file());
            w.write_all(&self.header())
                .and_then(|()| self.sections.iter().try_for_each(|s| w.write_all(s)))
                .and_then(|()| w.flush())
                .map_err(|e| Error::io(path, e))?;
        }
        af.commit()
    }
}

/// A packed string column under construction: u64 end-offsets
/// (`offsets[i]..offsets[i+1]` frames string i) plus the concatenated bytes.
#[derive(Default)]
struct StrColBuilder {
    bounds: Vec<u64>,
    bytes: Vec<u8>,
}

impl StrColBuilder {
    fn with_capacity(n: usize) -> StrColBuilder {
        let mut bounds = Vec::with_capacity(n + 1);
        bounds.push(0);
        StrColBuilder {
            bounds,
            bytes: Vec::new(),
        }
    }

    fn push(&mut self, s: &str) {
        self.bytes.extend_from_slice(s.as_bytes());
        self.bounds.push(self.bytes.len() as u64);
    }

    /// Push `s` lowercased (ASCII) without allocating an intermediate String.
    fn push_ascii_lower(&mut self, s: &str) {
        let at = self.bytes.len();
        self.bytes.extend_from_slice(s.as_bytes());
        self.bytes[at..].make_ascii_lowercase();
        self.bounds.push(self.bytes.len() as u64);
    }

    /// Borrow string `i`. Only ever called on strings this builder pushed, so
    /// the bounds are valid UTF-8 boundaries (ASCII lowercasing preserves
    /// them).
    fn get(&self, i: u32) -> &str {
        let lo = self.bounds[i as usize] as usize;
        let hi = self.bounds[i as usize + 1] as usize;
        std::str::from_utf8(&self.bytes[lo..hi]).unwrap_or("")
    }

    fn into_sections(self) -> (Vec<u8>, Vec<u8>) {
        let mut offsets = Vec::with_capacity(self.bounds.len() * 8);
        for b in &self.bounds {
            offsets.extend_from_slice(&b.to_le_bytes());
        }
        (offsets, self.bytes)
    }
}

/// Doc CSR under construction: rows must arrive sorted by `doc_id`; closing
/// against the final `doc_count` yields u32 start offsets of length
/// `doc_count + 1`.
struct DocStartsBuilder {
    starts: Vec<u8>,
    cur_doc: u64,
    rows: u32,
}

impl DocStartsBuilder {
    fn new() -> DocStartsBuilder {
        DocStartsBuilder {
            starts: 0u32.to_le_bytes().to_vec(),
            cur_doc: 0,
            rows: 0,
        }
    }

    fn push(&mut self, doc_id: u32) -> Result<()> {
        if (doc_id as u64) < self.cur_doc {
            return Err(Error::other("table rows not sorted by doc id"));
        }
        while self.cur_doc < doc_id as u64 {
            self.starts.extend_from_slice(&self.rows.to_le_bytes());
            self.cur_doc += 1;
        }
        self.rows += 1;
        Ok(())
    }

    fn finish(mut self, doc_count: usize) -> Result<Vec<u8>> {
        if self.rows > 0 && self.cur_doc >= doc_count as u64 {
            return Err(Error::other("table row references out-of-range doc id"));
        }
        while self.cur_doc < doc_count as u64 {
            self.starts.extend_from_slice(&self.rows.to_le_bytes());
            self.cur_doc += 1;
        }
        Ok(self.starts)
    }
}

/// Name FST + ids blob: maps each distinct key to `(start << 32) | count`
/// addressing a run of u32 row ids (ascending within a run).
///
/// Built by sorting row ids by `(name, id)` and streaming runs into the FST
/// builder — no per-name maps or per-row String allocations.
fn build_name_fst<'a, F>(n: u32, name_of: F) -> Result<(Vec<u8>, Vec<u8>)>
where
    F: Fn(u32) -> &'a str + Sync,
{
    use rayon::prelude::*;
    let mut order: Vec<u32> = (0..n).collect();
    order.par_sort_unstable_by(|&a, &b| name_of(a).cmp(name_of(b)).then(a.cmp(&b)));

    let mut ids: Vec<u8> = Vec::with_capacity(n as usize * 4);
    let mut builder = fst::MapBuilder::memory();
    let mut i = 0usize;
    while i < order.len() {
        let name = name_of(order[i]);
        let start = i as u64;
        while i < order.len() && name_of(order[i]) == name {
            ids.extend_from_slice(&order[i].to_le_bytes());
            i += 1;
        }
        builder
            .insert(name.as_bytes(), (start << 32) | (i as u64 - start))
            .map_err(|e| Error::other(format!("name fst: {e}")))?;
    }
    let fst_bytes = builder
        .into_inner()
        .map_err(|e| Error::other(format!("name fst: {e}")))?;
    Ok((fst_bytes, ids))
}

/// Incrementally builds a columnar symbol table, row by row, in doc order.
///
/// The segment writer and the compaction merge both feed this directly, so a
/// build never materializes an intermediate `Vec<SymbolEntry>` (millions of
/// heap Strings on big trees) — bytes go straight into the packed sections.
pub(crate) struct SymTableBuilder {
    row_offsets: Vec<u8>,
    row_bytes: Vec<u8>,
    csr: DocStartsBuilder,
    names: StrColBuilder,
    lowers: StrColBuilder,
    kinds: Vec<String>,
    kind_ids: HashMap<String, u32>,
    rows: u32,
}

impl SymTableBuilder {
    pub fn new() -> SymTableBuilder {
        SymTableBuilder {
            row_offsets: 0u64.to_le_bytes().to_vec(),
            row_bytes: Vec::new(),
            csr: DocStartsBuilder::new(),
            names: StrColBuilder::with_capacity(0),
            lowers: StrColBuilder::with_capacity(0),
            kinds: Vec::new(),
            kind_ids: HashMap::new(),
            rows: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.rows as usize
    }

    /// Append a symbol row. `doc_id`s must be non-decreasing across calls.
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        doc_id: u32,
        name: &str,
        kind: &str,
        line_start: u32,
        line_end: u32,
        container: Option<&str>,
        signature: Option<&str>,
    ) -> Result<()> {
        self.csr.push(doc_id)?;
        let kind_id = match self.kind_ids.get(kind) {
            Some(&id) => id,
            None => {
                let id = self.kinds.len() as u32;
                self.kinds.push(kind.to_string());
                self.kind_ids.insert(kind.to_string(), id);
                id
            }
        };
        let row = SymRowRef {
            doc_id,
            kind_id,
            line_start,
            line_end,
            container,
            signature,
        };
        postcard::to_io(&row, &mut self.row_bytes)?;
        self.row_offsets
            .extend_from_slice(&(self.row_bytes.len() as u64).to_le_bytes());
        self.names.push(name);
        self.lowers.push_ascii_lower(name);
        self.rows += 1;
        Ok(())
    }

    pub fn finish(self, doc_count: usize) -> Result<EncodedTable> {
        let doc_starts = self.csr.finish(doc_count)?;
        let (fst_bytes, fst_rows) = build_name_fst(self.rows, |i| self.lowers.get(i))?;
        let kinds_bytes = postcard::to_allocvec(&self.kinds)?;
        let (name_offsets, name_bytes) = self.names.into_sections();
        let (lower_offsets, lower_bytes) = self.lowers.into_sections();

        Ok(EncodedTable {
            magic: SYM_MAGIC,
            row_count: self.rows,
            doc_count: doc_count as u32,
            sections: vec![
                self.row_offsets,
                self.row_bytes,
                doc_starts,
                name_offsets,
                name_bytes,
                lower_offsets,
                lower_bytes,
                fst_bytes,
                fst_rows,
                kinds_bytes,
            ],
        })
    }
}

/// Incrementally builds a columnar reference table (see [`SymTableBuilder`]).
pub(crate) struct RefTableBuilder {
    row_offsets: Vec<u8>,
    row_bytes: Vec<u8>,
    csr: DocStartsBuilder,
    names: StrColBuilder,
    rows: u32,
}

impl RefTableBuilder {
    pub fn new() -> RefTableBuilder {
        RefTableBuilder {
            row_offsets: 0u64.to_le_bytes().to_vec(),
            row_bytes: Vec::new(),
            csr: DocStartsBuilder::new(),
            names: StrColBuilder::with_capacity(0),
            rows: 0,
        }
    }

    /// Append a reference row. `doc_id`s must be non-decreasing across calls.
    pub fn push(
        &mut self,
        doc_id: u32,
        name: &str,
        kind: RefKind,
        line: u32,
        column: u32,
    ) -> Result<()> {
        self.csr.push(doc_id)?;
        let row = RefRow {
            doc_id,
            kind,
            line,
            column,
        };
        postcard::to_io(&row, &mut self.row_bytes)?;
        self.row_offsets
            .extend_from_slice(&(self.row_bytes.len() as u64).to_le_bytes());
        self.names.push(name);
        self.rows += 1;
        Ok(())
    }

    pub fn finish(self, doc_count: usize) -> Result<EncodedTable> {
        let doc_starts = self.csr.finish(doc_count)?;
        let (fst_bytes, fst_rows) = build_name_fst(self.rows, |i| self.names.get(i))?;
        let (name_offsets, name_bytes) = self.names.into_sections();

        Ok(EncodedTable {
            magic: REF_MAGIC,
            row_count: self.rows,
            doc_count: doc_count as u32,
            sections: vec![
                self.row_offsets,
                self.row_bytes,
                doc_starts,
                name_offsets,
                name_bytes,
                fst_bytes,
                fst_rows,
            ],
        })
    }
}

/// Encode a symbol table from materialized rows (sorted by `doc_id`).
/// Convenience wrapper over [`SymTableBuilder`]; production paths feed the
/// builder incrementally instead.
#[cfg(test)]
pub(crate) fn encode_syms(rows: &[SymbolEntry], doc_count: usize) -> Result<EncodedTable> {
    let mut b = SymTableBuilder::new();
    for s in rows {
        b.push(
            s.doc_id,
            &s.name,
            &s.kind,
            s.line_start,
            s.line_end,
            s.container.as_deref(),
            s.signature.as_deref(),
        )?;
    }
    b.finish(doc_count)
}

/// Encode a reference table from materialized rows (sorted by `doc_id`).
#[cfg(test)]
pub(crate) fn encode_refs(rows: &[RefEntry], doc_count: usize) -> Result<EncodedTable> {
    let mut b = RefTableBuilder::new();
    for r in rows {
        b.push(r.doc_id, &r.name, r.kind, r.line, r.column)?;
    }
    b.finish(doc_count)
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A section of the mmap that the FST crate can borrow as `[u8]`.
struct SectionBytes {
    mmap: Arc<Mmap>,
    range: Range<usize>,
}

impl AsRef<[u8]> for SectionBytes {
    fn as_ref(&self) -> &[u8] {
        &self.mmap[self.range.clone()]
    }
}

/// Parse and validate a table file's header, returning
/// `(row_count, doc_count, section ranges)`. All sections except `SEC_ROWS`
/// are checksum-verified here.
fn parse_header(
    mmap: &Mmap,
    magic: [u8; 4],
    n_sections: usize,
    what: &str,
) -> Result<(u32, u32, Vec<Range<usize>>)> {
    let corrupt = |m: &str| Error::Corrupt(format!("{what}: {m}"));
    let head_len = 12 + n_sections * 24;
    let head = mmap
        .get(..head_len)
        .ok_or_else(|| corrupt("truncated header"))?;
    if head[..4] != magic {
        return Err(corrupt("bad magic"));
    }
    let u32_at = |o: usize| u32::from_le_bytes(head[o..o + 4].try_into().expect("4 bytes"));
    let u64_at = |o: usize| u64::from_le_bytes(head[o..o + 8].try_into().expect("8 bytes"));
    let row_count = u32_at(4);
    let doc_count = u32_at(8);

    let mut sections = Vec::with_capacity(n_sections);
    for i in 0..n_sections {
        let e = 12 + i * 24;
        let offset = u64_at(e) as usize;
        let len = u64_at(e + 8) as usize;
        let sum = u64_at(e + 16);
        let end = offset
            .checked_add(len)
            .ok_or_else(|| corrupt("section overflow"))?;
        let bytes = mmap
            .get(offset..end)
            .ok_or_else(|| corrupt("section out of range"))?;
        // Row data dominates the file; it is bounds-checked per row instead of
        // paged in wholesale here, keeping open O(small sections).
        if i != SEC_ROWS && xxhash_rust::xxh3::xxh3_64(bytes) != sum {
            return Err(corrupt("section checksum mismatch"));
        }
        sections.push(offset..end);
    }
    Ok((row_count, doc_count, sections))
}

/// Shared accessor plumbing over a parsed table file.
struct TableCore {
    mmap: Arc<Mmap>,
    row_count: u32,
    doc_count: u32,
    sections: Vec<Range<usize>>,
}

impl TableCore {
    fn open(path: &Path, magic: [u8; 4], n_sections: usize, what: &str) -> Result<TableCore> {
        let file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
        let mmap = unsafe { Mmap::map(&file).map_err(|e| Error::io(path, e))? };
        let (row_count, doc_count, sections) = parse_header(&mmap, magic, n_sections, what)?;
        Ok(TableCore {
            mmap: Arc::new(mmap),
            row_count,
            doc_count,
            sections,
        })
    }

    fn section(&self, i: usize) -> &[u8] {
        &self.mmap[self.sections[i].clone()]
    }

    /// Validate that a fixed-stride section has exactly the length implied by
    /// the header counts. The header's `row_count`/`doc_count` carry no
    /// checksum of their own; tying them to the checksummed sections makes a
    /// corrupted count fail open instead of degrading silently.
    fn check_stride(&self, sec: usize, elem: usize, count: usize, what: &str) -> Result<()> {
        let want = elem * (count + 1);
        let got = self.sections[sec].len();
        if got != want {
            return Err(Error::Corrupt(format!(
                "{what}: section {sec} is {got} bytes, expected {want}"
            )));
        }
        Ok(())
    }

    fn u64_entry(&self, sec: usize, i: usize) -> Option<u64> {
        let s = self.section(sec);
        let b = s.get(i * 8..i * 8 + 8)?;
        Some(u64::from_le_bytes(b.try_into().expect("8 bytes")))
    }

    fn u32_entry(&self, sec: usize, i: usize) -> Option<u32> {
        let s = self.section(sec);
        let b = s.get(i * 4..i * 4 + 4)?;
        Some(u32::from_le_bytes(b.try_into().expect("4 bytes")))
    }

    /// The undecoded postcard bytes of row `i` (frame offsets are from a
    /// verified section; the row bytes themselves are bounds-checked).
    fn row_bytes(&self, i: u32) -> Option<&[u8]> {
        if i >= self.row_count {
            return None;
        }
        let lo = self.u64_entry(SEC_ROW_OFFSETS, i as usize)? as usize;
        let hi = self.u64_entry(SEC_ROW_OFFSETS, i as usize + 1)? as usize;
        self.section(SEC_ROWS).get(lo..hi)
    }

    /// String `i` of the column whose offsets/bytes sections are given.
    fn str_at(&self, off_sec: usize, bytes_sec: usize, i: u32) -> &str {
        let lo = self.u64_entry(off_sec, i as usize).unwrap_or(0) as usize;
        let hi = self.u64_entry(off_sec, i as usize + 1).unwrap_or(0) as usize;
        self.section(bytes_sec)
            .get(lo..hi)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }

    /// Row range belonging to `doc_id` (rows are stored sorted by doc).
    fn doc_range(&self, doc_id: u32) -> Range<u32> {
        if doc_id >= self.doc_count {
            return 0..0;
        }
        let lo = self.u32_entry(SEC_DOC_STARTS, doc_id as usize).unwrap_or(0);
        let hi = self
            .u32_entry(SEC_DOC_STARTS, doc_id as usize + 1)
            .unwrap_or(lo);
        lo..hi.max(lo)
    }

    /// Row ids for a name-FST hit: `value` packs `(start << 32) | count` into
    /// the ids section.
    fn fst_row_ids(&self, ids_sec: usize, value: u64) -> impl Iterator<Item = u32> + '_ {
        let start = (value >> 32) as usize;
        let count = (value & 0xFFFF_FFFF) as usize;
        let s = self.section(ids_sec);
        (start..start + count).filter_map(move |i| {
            let b = s.get(i * 4..i * 4 + 4)?;
            Some(u32::from_le_bytes(b.try_into().expect("4 bytes")))
        })
    }
}

/// Read-only view of a columnar symbol table.
pub(crate) struct SymTable {
    core: TableCore,
    fst: fst::Map<SectionBytes>,
    kinds: Vec<String>,
}

impl SymTable {
    pub fn open(path: &Path) -> Result<SymTable> {
        let core = TableCore::open(path, SYM_MAGIC, SYM_SECTIONS, "symbol table")?;
        let (rows, docs) = (core.row_count as usize, core.doc_count as usize);
        core.check_stride(SEC_ROW_OFFSETS, 8, rows, "symbol table")?;
        core.check_stride(SEC_DOC_STARTS, 4, docs, "symbol table")?;
        core.check_stride(SEC_NAME_OFFSETS, 8, rows, "symbol table")?;
        core.check_stride(SYM_SEC_LOWER_OFFSETS, 8, rows, "symbol table")?;
        let fst = fst::Map::new(SectionBytes {
            mmap: core.mmap.clone(),
            range: core.sections[SYM_SEC_FST].clone(),
        })?;
        let kinds: Vec<String> = postcard::from_bytes(core.section(SYM_SEC_KINDS))?;
        Ok(SymTable { core, fst, kinds })
    }

    pub fn len(&self) -> usize {
        self.core.row_count as usize
    }

    /// Number of documents this table was written against.
    pub fn doc_count(&self) -> usize {
        self.core.doc_count as usize
    }

    /// Decode symbol `i`. A corrupt row decodes to `None` (logged) instead of
    /// failing the whole query; the framing offsets are checksum-verified so
    /// damage cannot spread to neighboring rows.
    pub fn get(&self, i: u32) -> Option<SymbolEntry> {
        let row: SymRow = match postcard::from_bytes(self.core.row_bytes(i)?) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("corrupt symbol row {i}: {e}");
                return None;
            }
        };
        Some(SymbolEntry {
            doc_id: row.doc_id,
            name: self.name(i).to_string(),
            kind: self
                .kinds
                .get(row.kind_id as usize)
                .cloned()
                .unwrap_or_default(),
            line_start: row.line_start,
            line_end: row.line_end,
            container: row.container,
            signature: row.signature,
        })
    }

    pub fn name(&self, i: u32) -> &str {
        self.core.str_at(SEC_NAME_OFFSETS, SEC_NAME_BYTES, i)
    }

    pub fn name_lower(&self, i: u32) -> &str {
        self.core
            .str_at(SYM_SEC_LOWER_OFFSETS, SYM_SEC_LOWER_BYTES, i)
    }

    /// `(row id, name, lowercased name)` for every symbol — the fuzzy-match
    /// scan path. Walks the packed string columns only; no row is decoded.
    pub fn names(&self) -> impl Iterator<Item = (u32, &str, &str)> {
        (0..self.core.row_count).map(move |i| (i, self.name(i), self.name_lower(i)))
    }

    pub fn doc_range(&self, doc_id: u32) -> Range<u32> {
        self.core.doc_range(doc_id)
    }

    /// Row ids whose lowercased name is exactly `lower` (O(results)).
    pub fn rows_named(&self, lower: &str) -> impl Iterator<Item = u32> + '_ {
        self.fst
            .get(lower.as_bytes())
            .into_iter()
            .flat_map(|v| self.core.fst_row_ids(SYM_SEC_FST_ROWS, v))
    }
}

/// Read-only view of a columnar reference table.
pub(crate) struct RefTable {
    core: TableCore,
    fst: fst::Map<SectionBytes>,
}

impl RefTable {
    pub fn open(path: &Path) -> Result<RefTable> {
        let core = TableCore::open(path, REF_MAGIC, REF_SECTIONS, "ref table")?;
        let (rows, docs) = (core.row_count as usize, core.doc_count as usize);
        core.check_stride(SEC_ROW_OFFSETS, 8, rows, "ref table")?;
        core.check_stride(SEC_DOC_STARTS, 4, docs, "ref table")?;
        core.check_stride(SEC_NAME_OFFSETS, 8, rows, "ref table")?;
        let fst = fst::Map::new(SectionBytes {
            mmap: core.mmap.clone(),
            range: core.sections[REF_SEC_FST].clone(),
        })?;
        Ok(RefTable { core, fst })
    }

    /// Number of documents this table was written against.
    pub fn doc_count(&self) -> usize {
        self.core.doc_count as usize
    }

    pub fn get(&self, i: u32) -> Option<RefEntry> {
        let row: RefRow = match postcard::from_bytes(self.core.row_bytes(i)?) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("corrupt ref row {i}: {e}");
                return None;
            }
        };
        Some(RefEntry {
            doc_id: row.doc_id,
            name: self.core.str_at(SEC_NAME_OFFSETS, SEC_NAME_BYTES, i).to_string(),
            kind: row.kind,
            line: row.line,
            column: row.column,
        })
    }

    pub fn doc_range(&self, doc_id: u32) -> Range<u32> {
        self.core.doc_range(doc_id)
    }

    /// Row ids whose name is exactly `name` (O(results)).
    pub fn rows_named(&self, name: &str) -> impl Iterator<Item = u32> + '_ {
        self.fst
            .get(name.as_bytes())
            .into_iter()
            .flat_map(|v| self.core.fst_row_ids(REF_SEC_FST_ROWS, v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_syms() -> Vec<SymbolEntry> {
        vec![
            SymbolEntry {
                doc_id: 0,
                name: "Alpha".into(),
                kind: "struct".into(),
                line_start: 1,
                line_end: 10,
                container: None,
                signature: Some("struct Alpha".into()),
            },
            SymbolEntry {
                doc_id: 0,
                name: "beta".into(),
                kind: "function".into(),
                line_start: 12,
                line_end: 20,
                container: Some("Alpha".into()),
                signature: None,
            },
            SymbolEntry {
                doc_id: 2,
                name: "alpha".into(),
                kind: "function".into(),
                line_start: 3,
                line_end: 4,
                container: None,
                signature: None,
            },
        ]
    }

    fn write_tmp(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("greplm-table-{tag}-{nanos}"));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// On-disk bytes of an encoded table, for corruption tests.
    fn table_bytes(t: &EncodedTable) -> Vec<u8> {
        let mut out = t.header();
        for s in &t.sections {
            out.extend_from_slice(s);
        }
        out
    }

    #[test]
    fn syms_roundtrip_with_csr_and_name_index() {
        let rows = sample_syms();
        let bytes = table_bytes(&encode_syms(&rows, 3).unwrap());
        let path = write_tmp("syms", &bytes);
        let t = SymTable::open(&path).unwrap();

        assert_eq!(t.len(), 3);
        for (i, want) in rows.iter().enumerate() {
            let got = t.get(i as u32).expect("row decodes");
            assert_eq!(got.doc_id, want.doc_id);
            assert_eq!(got.name, want.name);
            assert_eq!(got.kind, want.kind);
            assert_eq!(got.line_start, want.line_start);
            assert_eq!(got.container, want.container);
            assert_eq!(got.signature, want.signature);
        }
        // CSR: doc 0 has rows 0..2, doc 1 none, doc 2 row 2.
        assert_eq!(t.doc_range(0), 0..2);
        assert_eq!(t.doc_range(1), 2..2);
        assert_eq!(t.doc_range(2), 2..3);
        assert_eq!(t.doc_range(99), 0..0);
        // Name index folds case: "alpha" matches rows 0 and 2.
        let hits: Vec<u32> = t.rows_named("alpha").collect();
        assert_eq!(hits, vec![0, 2]);
        assert!(t.rows_named("nope").next().is_none());
        // Name columns.
        assert_eq!(t.name(0), "Alpha");
        assert_eq!(t.name_lower(0), "alpha");
        assert_eq!(t.names().count(), 3);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refs_roundtrip() {
        let rows = vec![
            RefEntry {
                doc_id: 0,
                name: "call_me".into(),
                kind: RefKind::Call,
                line: 5,
                column: 9,
            },
            RefEntry {
                doc_id: 1,
                name: "call_me".into(),
                kind: RefKind::Import,
                line: 1,
                column: 1,
            },
        ];
        let bytes = table_bytes(&encode_refs(&rows, 2).unwrap());
        let path = write_tmp("refs", &bytes);
        let t = RefTable::open(&path).unwrap();

        let hits: Vec<RefEntry> = t.rows_named("call_me").filter_map(|i| t.get(i)).collect();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].kind, RefKind::Call);
        assert_eq!(hits[1].doc_id, 1);
        assert_eq!(t.doc_range(1), 1..2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_tables_open() {
        let bytes = table_bytes(&encode_syms(&[], 0).unwrap());
        let path = write_tmp("empty", &bytes);
        let t = SymTable::open(&path).unwrap();
        assert_eq!(t.len(), 0);
        assert!(t.get(0).is_none());
        assert!(t.rows_named("x").next().is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_row_data_degrades_without_panicking() {
        let rows = sample_syms();
        let mut bytes = table_bytes(&encode_syms(&rows, 3).unwrap());
        // Row data is the one lazily-verified section: flip a byte inside it.
        // Open must still succeed and accessors must not panic; the damaged
        // row either fails decode (None) or yields garbage fields, but the
        // framing offsets are verified so neighbors are unaffected.
        let e = 12 + SEC_ROWS * 24;
        let off = u64::from_le_bytes(bytes[e..e + 8].try_into().unwrap()) as usize;
        bytes[off] ^= 0xFF;
        let path = write_tmp("rowflip", &bytes);
        let t = SymTable::open(&path).expect("open ignores row-data damage");
        for i in 0..t.len() as u32 {
            let _ = t.get(i); // must not panic
        }
        assert_eq!(t.get(1).map(|s| s.name), Some("beta".into()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_header_sections_fail_open() {
        let rows = sample_syms();
        let bytes = table_bytes(&encode_syms(&rows, 3).unwrap());

        // Truncation fails.
        let path = write_tmp("trunc", &bytes[..bytes.len() / 2]);
        assert!(SymTable::open(&path).is_err());
        let _ = std::fs::remove_file(&path);

        // A flipped bit in a verified section (the row offsets, right after
        // the header) fails the checksum at open.
        let mut bad = bytes.clone();
        let off = 12 + SYM_SECTIONS * 24 + 4; // inside row_offsets
        bad[off] ^= 0xFF;
        let path = write_tmp("flip", &bad);
        assert!(matches!(SymTable::open(&path), Err(Error::Corrupt(_))));
        let _ = std::fs::remove_file(&path);

        // A corrupted header count (no checksum of its own) is caught by the
        // structural cross-check against the verified section lengths.
        let mut bad = bytes.clone();
        bad[4] ^= 0xFF; // row_count low byte
        let path = write_tmp("count", &bad);
        assert!(matches!(SymTable::open(&path), Err(Error::Corrupt(_))));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn out_of_range_doc_id_fails_encode() {
        let mut rows = sample_syms();
        rows[2].doc_id = 7;
        assert!(encode_syms(&rows, 3).is_err());
    }

    #[test]
    fn streamed_write_matches_assembled_bytes() {
        let enc = encode_syms(&sample_syms(), 3).unwrap();
        let mut p = std::env::temp_dir();
        p.push(format!(
            "greplm-table-stream-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        enc.write_atomic(&p).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), table_bytes(&enc));
        let _ = std::fs::remove_file(&p);
    }
}
