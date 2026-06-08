//! Query execution: trigram candidate filtering, then exact verification.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use lru::LruCache;
use memchr::memmem;
use rayon::prelude::*;
use regex::bytes::Regex as BytesRegex;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::lang::Language;
use crate::meta::Meta;
use crate::paths::Paths;
use crate::segment::{RefKind, Segment};
use crate::trigram::{self, TrigramQuery};

/// A content search request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchQuery {
    pub pattern: String,
    pub regex: bool,
    pub case_insensitive: bool,
    /// Match only whole identifiers (word boundaries on both sides).
    pub whole_word: bool,
    pub lang: Option<String>,
    pub path: Option<String>,
    pub limit: usize,
    /// Skip the first N ranked results (for pagination).
    pub offset: usize,
    pub max_per_file: usize,
    /// Return EVERY match in deterministic (path, line) order: no ranking, no
    /// global `limit`, and no per-file caps (`max_per_file` and the internal
    /// pathological-input cap are both lifted). This is grep-equivalent
    /// completeness; use it when "find every occurrence" matters more than
    /// relevance ranking. `offset`/`limit` are ignored when set.
    pub exhaustive: bool,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            regex: false,
            case_insensitive: false,
            whole_word: false,
            lang: None,
            path: None,
            limit: 50,
            offset: 0,
            max_per_file: 20,
            exhaustive: false,
        }
    }
}

/// A single content match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub path: String,
    pub lang: String,
    pub line: u32,
    pub column: u32,
    pub text: String,
    pub score: f32,
}

/// A symbol lookup request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SymbolQuery {
    pub name: String,
    pub kind: Option<String>,
    pub exact: bool,
    pub limit: usize,
    pub offset: usize,
}

impl Default for SymbolQuery {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: None,
            exact: false,
            limit: 50,
            offset: 0,
        }
    }
}

/// A single symbol match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolHit {
    pub path: String,
    pub lang: String,
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub score: f32,
}

/// A resolved reference to an identifier: a definition, a call site, or an
/// import. Unlike text search, these come from the structural reference index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefHit {
    pub path: String,
    pub lang: String,
    pub name: String,
    /// "definition", "call", or "import".
    pub kind: String,
    pub line: u32,
    pub column: u32,
    /// The enclosing symbol at this location, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

/// One edge of the call graph: a call site linking a caller symbol to a callee.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallSite {
    /// The enclosing symbol the call is made from (None at file scope).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    /// The called identifier.
    pub callee: String,
    pub path: String,
    pub lang: String,
    pub line: u32,
    pub column: u32,
}

/// A symbol affected by a change to a target symbol, with its BFS distance from
/// the target along the reverse call graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactNode {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub lang: String,
    pub line_start: u32,
    pub line_end: u32,
    /// Hops along the caller chain from the target (0 = the target itself).
    pub distance: u32,
}

/// A candidate definition for an identifier at a source position, ranked by
/// resolution confidence. `resolved` marks a single high-confidence target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefHit {
    pub path: String,
    pub lang: String,
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub score: f32,
    /// True when this is the unambiguous resolution target.
    pub resolved: bool,
}

/// The git history of a resolved symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolHistory {
    pub name: String,
    pub path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub commits: Vec<crate::git::Commit>,
}

/// A changed file annotated with the symbols it defines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedSymbols {
    pub path: String,
    pub status: String,
    pub symbols: Vec<String>,
}

/// A structural (AST) search match, with its captured meta-variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructHit {
    pub path: String,
    pub lang: String,
    pub line_start: u32,
    pub line_end: u32,
    /// Kind of the matched node.
    pub kind: String,
    /// First line of the match, for display.
    pub text: String,
    pub captures: Vec<crate::structural::StructCapture>,
}

enum Matcher {
    Literal(Vec<u8>),
    Regex(BytesRegex),
}

impl Matcher {
    fn build(query: &SearchQuery) -> Result<Matcher> {
        if query.regex {
            let re = regex::bytes::RegexBuilder::new(&query.pattern)
                .case_insensitive(query.case_insensitive)
                .build()?;
            Ok(Matcher::Regex(re))
        } else if query.case_insensitive {
            let re = regex::bytes::RegexBuilder::new(&regex::escape(&query.pattern))
                .case_insensitive(true)
                .build()?;
            Ok(Matcher::Regex(re))
        } else {
            Ok(Matcher::Literal(query.pattern.as_bytes().to_vec()))
        }
    }

    /// Collect the byte offsets of matches in `hay`, up to `cap`. Scanning the
    /// whole buffer (rather than line-by-line) lets regex patterns span newlines.
    /// When `whole_word` is set, only matches bounded by non-identifier bytes
    /// count.
    fn match_starts(&self, hay: &[u8], whole_word: bool, cap: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        match self {
            Matcher::Literal(needle) => {
                if needle.is_empty() {
                    return out;
                }
                for pos in memmem::find_iter(hay, needle) {
                    let end = pos + needle.len();
                    if !whole_word || boundary_ok(hay, pos, end) {
                        out.push((pos, end));
                        if out.len() >= cap {
                            break;
                        }
                    }
                }
            }
            Matcher::Regex(re) => {
                for m in re.find_iter(hay) {
                    // Skip zero-width matches (e.g. `a*`, `^`): they carry no
                    // displayable span and would flag every line.
                    if m.start() == m.end() {
                        continue;
                    }
                    if !whole_word || boundary_ok(hay, m.start(), m.end()) {
                        out.push((m.start(), m.end()));
                        if out.len() >= cap {
                            break;
                        }
                    }
                }
            }
        }
        out
    }
}

/// Identifier byte for word-boundary checks. Bytes >= 0x80 are treated as
/// identifier bytes so multibyte UTF-8 (Unicode) identifiers are respected.
fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric() || b >= 0x80
}

/// True if the byte range `[start, end)` is bounded by non-identifier bytes.
fn boundary_ok(line: &[u8], start: usize, end: usize) -> bool {
    let left = start == 0 || !is_ident_byte(line[start - 1]);
    let right = end >= line.len() || !is_ident_byte(line[end]);
    left && right
}

/// Memory budget (in bytes) for the verification content cache. Eviction is
/// driven by total cached bytes rather than a file count, so a query that
/// touches many large files can't balloon resident memory. The cache is
/// content-addressed by hash, so stale entries fall out when files change and
/// are re-indexed. ~256 MiB.
const CONTENT_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// Hard cap on matches collected per file before ranking, to bound work on
/// pathological inputs (e.g. a minified file where every line matches).
const PER_FILE_MATCH_CAP: usize = 4096;

struct CacheInner {
    map: LruCache<u64, Arc<[u8]>>,
    bytes: u64,
}

/// A thread-safe, content-addressed, byte-budgeted cache of recently read
/// files. Entries are evicted least-recently-used until total cached bytes fit
/// within the budget.
struct ContentCache {
    inner: Mutex<CacheInner>,
    budget: u64,
}

impl ContentCache {
    fn new(budget_bytes: u64) -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                map: LruCache::unbounded(),
                bytes: 0,
            }),
            budget: budget_bytes.max(1),
        }
    }

    /// Return the bytes for `path`, reusing a cached copy keyed by `hash`. The
    /// file is read outside the lock so concurrent verifiers don't serialize.
    fn get_or_read(&self, hash: u64, path: &Path) -> Option<Arc<[u8]>> {
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(v) = guard.map.get(&hash) {
                return Some(v.clone());
            }
        }
        let data = std::fs::read(path).ok()?;
        let arc: Arc<[u8]> = Arc::from(data.into_boxed_slice());
        let len = arc.len() as u64;
        if let Ok(mut guard) = self.inner.lock() {
            // A single file larger than the whole budget is returned but not
            // cached; storing it would just evict everything else and itself.
            if len <= self.budget {
                if let Some(prev) = guard.map.put(hash, arc.clone()) {
                    guard.bytes = guard.bytes.saturating_sub(prev.len() as u64);
                }
                guard.bytes += len;
                while guard.bytes > self.budget {
                    match guard.map.pop_lru() {
                        Some((_, evicted)) => {
                            guard.bytes = guard.bytes.saturating_sub(evicted.len() as u64);
                        }
                        None => break,
                    }
                }
            }
        }
        Some(arc)
    }
}

/// Loaded, searchable index.
pub struct Searcher {
    paths: Paths,
    segments: Vec<Segment>,
    content: ContentCache,
}

impl Searcher {
    /// Open the index described by `meta`.
    pub fn open(paths: &Paths) -> Result<Searcher> {
        if !paths.exists() {
            return Err(Error::IndexMissing(paths.base.clone()));
        }
        let meta = Meta::load(&paths.meta_file())?;
        let mut segments = Vec::with_capacity(meta.segments.len());
        for &seg_id in &meta.segments {
            segments.push(Segment::open(paths, seg_id)?);
        }
        Ok(Searcher {
            paths: paths.clone(),
            segments,
            content: ContentCache::new(CONTENT_CACHE_BYTES),
        })
    }

    /// Run a content search.
    pub fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        if query.pattern.is_empty() {
            return Ok(Vec::new());
        }
        let matcher = Matcher::build(query)?;
        let tq: TrigramQuery = if query.regex {
            trigram::regex_trigrams(&query.pattern, query.case_insensitive)
        } else if query.case_insensitive {
            // Fold ASCII case into per-position trigram clauses so we still prune
            // candidates instead of scanning the whole repository.
            TrigramQuery::from_literal_ci(query.pattern.as_bytes())
        } else {
            TrigramQuery::from_literal(query.pattern.as_bytes())
        };

        let path_filter = query.path.as_deref();
        let lang_filter = query.lang.as_deref();

        // Gather candidate (segment, doc) pairs after cheap metadata filters.
        let mut targets: Vec<(usize, u32)> = Vec::new();
        for (si, seg) in self.segments.iter().enumerate() {
            let candidates = seg.candidates(&tq)?;
            for doc_id in candidates.iter() {
                if !seg.is_live(doc_id) {
                    continue;
                }
                let doc = match seg.doc(doc_id) {
                    Some(d) => d,
                    None => continue,
                };
                if let Some(lf) = lang_filter {
                    if doc.lang != lf {
                        continue;
                    }
                }
                if let Some(pf) = path_filter {
                    if !doc.path.contains(pf) {
                        continue;
                    }
                }
                targets.push((si, doc_id));
            }
        }

        // Verify candidates in parallel: each reads its file (cache/page-cache
        // backed) and scans the buffer with the real matcher.
        let root = &self.paths.root;
        let segments = &self.segments;
        let content = &self.content;
        let max_per_file = query.max_per_file;
        let whole_word = query.whole_word;
        let exhaustive = query.exhaustive;
        let mut hits: Vec<SearchHit> = targets
            .par_iter()
            .flat_map_iter(|&(si, doc_id)| {
                verify_doc(
                    &segments[si],
                    doc_id,
                    root,
                    content,
                    &matcher,
                    max_per_file,
                    whole_word,
                    exhaustive,
                )
                .into_iter()
            })
            .collect();

        if query.exhaustive {
            // Grep-equivalent: every match, deterministic (path, line, column)
            // order, no ranking and no offset/limit truncation.
            hits.sort_by(|a, b| {
                a.path
                    .cmp(&b.path)
                    .then_with(|| a.line.cmp(&b.line))
                    .then_with(|| a.column.cmp(&b.column))
            });
            return Ok(hits);
        }

        let cmp = |a: &SearchHit, b: &SearchHit| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        };
        Ok(rank_paginate(hits, cmp, query.offset, query.limit))
    }

    /// Look up symbols by name.
    pub fn symbols(&self, query: &SymbolQuery) -> Result<Vec<SymbolHit>> {
        let needle = query.name.to_ascii_lowercase();
        let mut hits: Vec<SymbolHit> = Vec::new();
        for seg in &self.segments {
            for (i, sym) in seg.syms.iter().enumerate() {
                if !seg.is_live(sym.doc_id) {
                    continue;
                }
                if let Some(k) = &query.kind {
                    if &sym.kind != k {
                        continue;
                    }
                }
                let score = match_symbol(&sym.name, seg.sym_name_lower(i), &needle, query.exact);
                let score = match score {
                    Some(s) => s,
                    None => continue,
                };
                let doc = match seg.doc(sym.doc_id) {
                    Some(d) => d,
                    None => continue,
                };
                let score = score + path_score(&doc.path);
                hits.push(SymbolHit {
                    path: doc.path.clone(),
                    lang: doc.lang.clone(),
                    name: sym.name.clone(),
                    kind: sym.kind.clone(),
                    line_start: sym.line_start,
                    line_end: sym.line_end,
                    container: sym.container.clone(),
                    signature: sym.signature.clone(),
                    score,
                });
            }
        }
        let cmp = |a: &SymbolHit, b: &SymbolHit| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.len().cmp(&b.name.len()))
                .then_with(|| a.path.cmp(&b.path))
        };
        Ok(rank_paginate(hits, cmp, query.offset, query.limit))
    }

    /// Return the symbol outline for a single file (by relative path).
    pub fn outline(&self, rel_path: &str) -> Result<Vec<SymbolHit>> {
        let mut out = Vec::new();
        for seg in &self.segments {
            for (doc_id, doc) in seg.docs.iter().enumerate() {
                let doc_id = doc_id as u32;
                if doc.path != rel_path || !seg.is_live(doc_id) {
                    continue;
                }
                for sym in seg.doc_syms(doc_id) {
                    out.push(SymbolHit {
                        path: doc.path.clone(),
                        lang: doc.lang.clone(),
                        name: sym.name.clone(),
                        kind: sym.kind.clone(),
                        line_start: sym.line_start,
                        line_end: sym.line_end,
                        container: sym.container.clone(),
                        signature: sym.signature.clone(),
                        score: 1.0,
                    });
                }
            }
        }
        out.sort_by_key(|s| s.line_start);
        Ok(out)
    }

    /// Find references to an identifier (whole-word occurrences across the repo).
    pub fn references(&self, name: &str, limit: usize, offset: usize) -> Result<Vec<SearchHit>> {
        self.search(&SearchQuery {
            pattern: name.to_string(),
            whole_word: true,
            limit,
            offset,
            ..Default::default()
        })
    }

    /// All live symbol definitions whose name matches `name` exactly
    /// (case-sensitive), as `(segment index, symbol index, symbol)` tuples.
    fn defs_by_name(&self, name: &str) -> Vec<(usize, usize, &crate::segment::SymbolEntry)> {
        let mut out = Vec::new();
        for (si, seg) in self.segments.iter().enumerate() {
            for (idx, sym) in seg.syms.iter().enumerate() {
                if sym.name == name && seg.is_live(sym.doc_id) {
                    out.push((si, idx, sym));
                }
            }
        }
        out
    }

    /// Number of live call sites targeting `name` (call-graph in-degree),
    /// computed via the per-segment callee-name index (no full ref scan).
    fn call_indegree(&self, name: &str) -> u32 {
        let mut n = 0u32;
        for seg in &self.segments {
            for r in seg.calls_to(name) {
                if seg.is_live(r.doc_id) {
                    n += 1;
                }
            }
        }
        n
    }

    /// The innermost symbol in `doc_id` whose line range contains `line`.
    fn enclosing_symbol<'s>(
        &self,
        seg: &'s Segment,
        doc_id: u32,
        line: u32,
    ) -> Option<&'s crate::segment::SymbolEntry> {
        let mut best: Option<&crate::segment::SymbolEntry> = None;
        for sym in seg.doc_syms(doc_id) {
            if sym.line_start <= line && line <= sym.line_end {
                let span = sym.line_end - sym.line_start;
                match best {
                    Some(b) if (b.line_end - b.line_start) <= span => {}
                    _ => best = Some(sym),
                }
            }
        }
        best
    }

    /// Resolved references to `name`: its definitions, call sites, and imports,
    /// drawn from the structural reference index (not text matching). Ranked
    /// definitions first, then calls, then imports.
    pub fn references_resolved(&self, name: &str, limit: usize, offset: usize) -> Vec<RefHit> {
        let mut hits: Vec<RefHit> = Vec::new();
        for seg in &self.segments {
            for sym in &seg.syms {
                if sym.name == name && seg.is_live(sym.doc_id) {
                    if let Some(doc) = seg.doc(sym.doc_id) {
                        hits.push(RefHit {
                            path: doc.path.clone(),
                            lang: doc.lang.clone(),
                            name: sym.name.clone(),
                            kind: "definition".to_string(),
                            line: sym.line_start,
                            column: 1,
                            container: sym.container.clone(),
                        });
                    }
                }
            }
            for r in &seg.refs {
                if r.name == name && seg.is_live(r.doc_id) {
                    if let Some(doc) = seg.doc(r.doc_id) {
                        let container = self
                            .enclosing_symbol(seg, r.doc_id, r.line)
                            .map(|s| s.name.clone());
                        hits.push(RefHit {
                            path: doc.path.clone(),
                            lang: doc.lang.clone(),
                            name: r.name.clone(),
                            kind: r.kind.as_str().to_string(),
                            line: r.line,
                            column: r.column,
                            container,
                        });
                    }
                }
            }
        }
        let rank = |k: &str| match k {
            "definition" => 0,
            "call" => 1,
            _ => 2,
        };
        hits.sort_by(|a, b| {
            rank(&a.kind)
                .cmp(&rank(&b.kind))
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        });
        paginate(hits, offset, limit)
    }

    /// Call sites *inside* `name`'s body: what `name` calls. Built by locating
    /// the definition(s) of `name` and collecting "call" refs within range.
    pub fn callees(&self, name: &str, limit: usize, offset: usize) -> Vec<CallSite> {
        let mut out: Vec<CallSite> = Vec::new();
        let mut seen: HashSet<(String, String, u32, u32)> = HashSet::new();
        for (si, _, sym) in self.defs_by_name(name) {
            let seg = &self.segments[si];
            let doc = match seg.doc(sym.doc_id) {
                Some(d) => d,
                None => continue,
            };
            for r in seg.doc_refs(sym.doc_id) {
                if r.kind == RefKind::Call && r.line >= sym.line_start && r.line <= sym.line_end {
                    let key = (doc.path.clone(), r.name.clone(), r.line, r.column);
                    if !seen.insert(key) {
                        continue;
                    }
                    out.push(CallSite {
                        caller: Some(name.to_string()),
                        callee: r.name.clone(),
                        path: doc.path.clone(),
                        lang: doc.lang.clone(),
                        line: r.line,
                        column: r.column,
                    });
                }
            }
        }
        out.sort_by(|a, b| {
            a.callee
                .cmp(&b.callee)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        });
        paginate(out, offset, limit)
    }

    /// Call sites that target `name`: who calls it. Each is attributed to its
    /// enclosing caller symbol when one can be determined.
    pub fn callers(&self, name: &str, limit: usize, offset: usize) -> Vec<CallSite> {
        let mut out: Vec<CallSite> = Vec::new();
        for seg in &self.segments {
            // O(results) via the prebuilt callee-name index instead of a full
            // scan of every ref — this is the inner loop of `blast_radius`.
            for r in seg.calls_to(name) {
                if !seg.is_live(r.doc_id) {
                    continue;
                }
                let doc = match seg.doc(r.doc_id) {
                    Some(d) => d,
                    None => continue,
                };
                let caller = self
                    .enclosing_symbol(seg, r.doc_id, r.line)
                    .map(|s| s.name.clone());
                out.push(CallSite {
                    caller,
                    callee: name.to_string(),
                    path: doc.path.clone(),
                    lang: doc.lang.clone(),
                    line: r.line,
                    column: r.column,
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));
        paginate(out, offset, limit)
    }

    /// Blast radius: the symbols transitively affected if `name` changes, found
    /// by walking the reverse call graph (callers, then their callers, ...) up
    /// to `depth` hops. Distance 0 is `name`'s own definition(s).
    ///
    /// Resolution is by name, so results are an approximation that can include
    /// unrelated same-named symbols; it is a guide, not a proof.
    pub fn blast_radius(&self, name: &str, depth: u32, limit: usize) -> Vec<ImpactNode> {
        let mut out: Vec<ImpactNode> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(name.to_string());

        // Distance 0: the target's own definitions.
        for (si, _, sym) in self.defs_by_name(name) {
            if let Some(doc) = self.segments[si].doc(sym.doc_id) {
                out.push(ImpactNode {
                    name: sym.name.clone(),
                    kind: sym.kind.clone(),
                    path: doc.path.clone(),
                    lang: doc.lang.clone(),
                    line_start: sym.line_start,
                    line_end: sym.line_end,
                    distance: 0,
                });
            }
        }

        let mut frontier: Vec<String> = vec![name.to_string()];
        for dist in 1..=depth {
            let mut next: Vec<String> = Vec::new();
            for target in &frontier {
                for site in self.callers(target, usize::MAX, 0) {
                    let caller = match site.caller {
                        Some(c) => c,
                        None => continue,
                    };
                    if !visited.insert(caller.clone()) {
                        continue;
                    }
                    for (si, _, sym) in self.defs_by_name(&caller) {
                        if let Some(doc) = self.segments[si].doc(sym.doc_id) {
                            out.push(ImpactNode {
                                name: sym.name.clone(),
                                kind: sym.kind.clone(),
                                path: doc.path.clone(),
                                lang: doc.lang.clone(),
                                line_start: sym.line_start,
                                line_end: sym.line_end,
                                distance: dist,
                            });
                        }
                    }
                    next.push(caller);
                }
                if out.len() >= limit {
                    break;
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        out.truncate(limit);
        out
    }

    /// Typed go-to-definition: resolve the identifier at `rel_path:line:col` to
    /// its most likely definition(s), combining scope/usage context with the
    /// global symbol table. Returns candidates ranked by confidence; the unique
    /// best is flagged `resolved`. Falls back to whole-word text hits (marked
    /// unresolved) when the name has no indexed definition.
    pub fn definition(&self, rel_path: &str, line: u32, col: u32) -> Result<Vec<DefHit>> {
        let full = self.resolve_within_root(rel_path)?;
        let source = std::fs::read(&full).map_err(|e| Error::io(&full, e))?;
        let ext = Path::new(rel_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let lang = crate::lang::Language::from_extension(ext);

        let ident = match crate::resolve::identifier_at(lang, &source, line, col) {
            Some(i) => i,
            None => {
                return Err(Error::other(format!(
                    "no identifier at {rel_path}:{line}:{col}"
                )))
            }
        };

        // Imports referenced by the use-file: a name imported here is likely
        // defined elsewhere, which lets us prefer cross-file definitions.
        let imported_here = self.imported_names(rel_path);

        let mut cands: Vec<DefHit> = Vec::new();
        for (si, _, sym) in self.defs_by_name(&ident.name) {
            let seg = &self.segments[si];
            let doc = match seg.doc(sym.doc_id) {
                Some(d) => d,
                None => continue,
            };
            let mut score = 10.0f32 + path_score(&doc.path);
            let same_file = doc.path == rel_path;
            if same_file {
                score += 40.0;
            }
            score += 2.0 * shared_prefix_len(rel_path, &doc.path) as f32;
            // Usage-context preference.
            let method_like = matches!(sym.kind.as_str(), "method" | "field" | "property");
            if ident.is_member && method_like {
                score += 25.0;
            } else if !ident.is_member && !method_like {
                score += 8.0;
            }
            if ident.is_call
                && matches!(
                    sym.kind.as_str(),
                    "function" | "method" | "macro" | "constructor"
                )
            {
                score += 6.0;
            }
            if ident.is_type
                && matches!(
                    sym.kind.as_str(),
                    "struct" | "class" | "interface" | "enum" | "type" | "trait" | "record"
                )
            {
                score += 12.0;
            }
            // If the name is imported into the use-file, a cross-file definition
            // is the likely target.
            if imported_here.contains(&ident.name) && !same_file {
                score += 15.0;
            }
            cands.push(DefHit {
                path: doc.path.clone(),
                lang: doc.lang.clone(),
                name: sym.name.clone(),
                kind: sym.kind.clone(),
                line_start: sym.line_start,
                line_end: sym.line_end,
                container: sym.container.clone(),
                signature: sym.signature.clone(),
                score,
                resolved: false,
            });
        }

        if cands.is_empty() {
            // Fallback: whole-word text occurrences, marked unresolved.
            let hits = self.references(&ident.name, 50, 0)?;
            return Ok(hits
                .into_iter()
                .map(|h| DefHit {
                    path: h.path,
                    lang: h.lang,
                    name: ident.name.clone(),
                    kind: "text".to_string(),
                    line_start: h.line,
                    line_end: h.line,
                    container: None,
                    signature: Some(h.text),
                    score: h.score,
                    resolved: false,
                })
                .collect());
        }

        cands.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line_start.cmp(&b.line_start))
        });
        // Mark the unique best as resolved when it clears the runner-up.
        let unique_top =
            cands.len() == 1 || (cands.len() >= 2 && cands[0].score - cands[1].score >= 12.0);
        if unique_top {
            cands[0].resolved = true;
        }
        Ok(cands)
    }

    /// Resolved references for the identifier at `rel_path:line:col`: its
    /// definitions, call sites, and imports across the repo.
    pub fn references_of(&self, rel_path: &str, line: u32, col: u32) -> Result<Vec<RefHit>> {
        let full = self.resolve_within_root(rel_path)?;
        let source = std::fs::read(&full).map_err(|e| Error::io(&full, e))?;
        let ext = Path::new(rel_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let lang = crate::lang::Language::from_extension(ext);
        let ident = crate::resolve::identifier_at(lang, &source, line, col)
            .ok_or_else(|| Error::other(format!("no identifier at {rel_path}:{line}:{col}")))?;
        Ok(self.references_resolved(&ident.name, usize::MAX, 0))
    }

    /// The set of names imported into `rel_path` (from the reference index).
    fn imported_names(&self, rel_path: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        for seg in &self.segments {
            for (doc_id, doc) in seg.docs.iter().enumerate() {
                let doc_id = doc_id as u32;
                if doc.path != rel_path || !seg.is_live(doc_id) {
                    continue;
                }
                for r in seg.doc_refs(doc_id) {
                    if r.kind == RefKind::Import {
                        out.insert(r.name.clone());
                    }
                }
            }
        }
        out
    }

    /// Resolve a caller-supplied path against the project root, rejecting
    /// anything that would escape it: absolute paths (which would make
    /// `root.join(..)` discard the root entirely), `..` traversal, and symlinks
    /// that resolve outside the tree. Returns the absolute path to read.
    fn resolve_within_root(&self, rel_path: &str) -> Result<PathBuf> {
        let candidate = Path::new(rel_path);
        if candidate.is_absolute() {
            return Err(Error::other(format!(
                "path {rel_path:?} must be relative to the project root"
            )));
        }
        // Reject parent/prefix components before touching the filesystem.
        if candidate
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(Error::other(format!(
                "path {rel_path:?} escapes the project root"
            )));
        }
        // Canonicalize both sides so symlinks can't redirect the read outside
        // the root, then require the resolved path to stay under it.
        let root = self
            .paths
            .root
            .canonicalize()
            .map_err(|e| Error::io(&self.paths.root, e))?;
        let full = root.join(candidate);
        let resolved = full.canonicalize().map_err(|e| Error::io(&full, e))?;
        if !resolved.starts_with(&root) {
            return Err(Error::other(format!(
                "path {rel_path:?} escapes the project root"
            )));
        }
        Ok(resolved)
    }

    /// Read a slice of a file with surrounding context lines.
    pub fn read_snippet(
        &self,
        rel_path: &str,
        start_line: u32,
        end_line: u32,
        context: u32,
    ) -> Result<Snippet> {
        let full = self.resolve_within_root(rel_path)?;
        let data = std::fs::read_to_string(&full).map_err(|e| Error::io(&full, e))?;
        let lines: Vec<&str> = data.lines().collect();
        let total = lines.len() as u32;
        let to = end_line.saturating_add(context).min(total.max(1));
        // Clamp the start into the file as well so an out-of-range request never
        // reports a `start_line` past EOF or an inverted (start > end) range.
        let from = start_line
            .saturating_sub(context)
            .max(1)
            .min(total.max(1))
            .min(to);
        let mut out = Vec::new();
        for ln in from..=to {
            if let Some(text) = lines.get((ln - 1) as usize) {
                out.push(SnippetLine {
                    line: ln,
                    text: (*text).to_string(),
                });
            }
        }
        Ok(Snippet {
            path: rel_path.to_string(),
            start_line: from,
            end_line: to,
            total_lines: total,
            lines: out,
        })
    }

    /// Build a token-budgeted context pack for `task`: the symbols (with
    /// signatures and code snippets) most relevant to the task, ranked by
    /// lexical relevance and call-graph centrality, plus their immediate
    /// dependency neighborhood. Designed to hand an agent exactly the code it
    /// needs without reading whole files.
    pub fn context_pack(&self, task: &str, budget_tokens: u64) -> crate::context::ContextPack {
        use crate::context::{self, ContextPack, PackItem};

        let terms = context::tokenize(task);

        // A candidate symbol with its location and provisional score.
        struct Cand {
            seg: usize,
            sym: usize,
            score: f32,
            reason: String,
        }
        let mut cands: Vec<Cand> = Vec::new();
        for (si, seg) in self.segments.iter().enumerate() {
            for (idx, sym) in seg.syms.iter().enumerate() {
                if !seg.is_live(sym.doc_id) {
                    continue;
                }
                let doc = match seg.doc(sym.doc_id) {
                    Some(d) => d,
                    None => continue,
                };
                let mut score = context::lexical_score(
                    &sym.name,
                    &sym.kind,
                    sym.signature.as_deref(),
                    sym.container.as_deref(),
                    &doc.path,
                    &terms,
                );
                if score <= 0.0 {
                    continue;
                }
                // Call-graph centrality, looked up only for the few symbols that
                // already cleared the lexical filter (via the call-name index).
                let deg = self.call_indegree(&sym.name) as f32;
                score += (1.0 + deg).ln() * 1.5;
                score += path_score(&doc.path);
                cands.push(Cand {
                    seg: si,
                    sym: idx,
                    score,
                    reason: "match".to_string(),
                });
            }
        }

        cands.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Expand the dependency neighborhood of the strongest seeds: include the
        // callees of the top matches so the agent sees what they depend on.
        let mut seen: HashSet<(String, u32)> = HashSet::new();
        for c in &cands {
            let seg = &self.segments[c.seg];
            let sym = &seg.syms[c.sym];
            seen.insert((sym.name.clone(), sym.line_start));
        }
        let mut extra: Vec<Cand> = Vec::new();
        for c in cands.iter().take(8) {
            let seg = &self.segments[c.seg];
            let sym = &seg.syms[c.sym];
            for callee in self.callees(&sym.name, 12, 0) {
                for (si2, idx, def) in self.defs_by_name(&callee.callee) {
                    let key = (def.name.clone(), def.line_start);
                    if !seen.insert(key) {
                        continue;
                    }
                    extra.push(Cand {
                        seg: si2,
                        sym: idx,
                        score: c.score * 0.3,
                        reason: format!("callee of {}", sym.name),
                    });
                }
            }
        }
        cands.extend(extra);
        cands.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Greedily pack within budget. Lines are read once per file through the
        // content cache and split once (cached by content hash), so multiple
        // packed symbols from the same file don't re-read or re-split it.
        let mut items: Vec<PackItem> = Vec::new();
        let mut used: u64 = 0;
        let mut truncated = false;
        let mut file_lines: std::collections::HashMap<u64, Arc<Vec<String>>> =
            std::collections::HashMap::new();
        const MAX_ITEM_LINES: u32 = 60;
        for c in &cands {
            let seg = &self.segments[c.seg];
            let sym = &seg.syms[c.sym];
            let doc = match seg.doc(sym.doc_id) {
                Some(d) => d,
                None => continue,
            };
            let end = sym
                .line_end
                .min(sym.line_start.saturating_add(MAX_ITEM_LINES));
            let lines = file_lines
                .entry(doc.hash)
                .or_insert_with(|| {
                    let full = self.paths.root.join(&doc.path);
                    let v = match self.content.get_or_read(doc.hash, &full) {
                        Some(data) => String::from_utf8_lossy(&data)
                            .lines()
                            .map(|s| s.to_string())
                            .collect(),
                        None => Vec::new(),
                    };
                    Arc::new(v)
                })
                .clone();
            let from = sym.line_start.max(1);
            let to = end.min(lines.len() as u32);
            let mut snippet = Vec::new();
            for ln in from..=to {
                if let Some(text) = lines.get((ln - 1) as usize) {
                    snippet.push(SnippetLine {
                        line: ln,
                        text: text.clone(),
                    });
                }
            }
            let chars: u64 = snippet.iter().map(|l| l.text.len() as u64 + 1).sum::<u64>()
                + sym.signature.as_ref().map(|s| s.len() as u64).unwrap_or(0);
            let cost = context::est_tokens(chars).max(1);
            if used + cost > budget_tokens && !items.is_empty() {
                truncated = true;
                continue;
            }
            used += cost;
            items.push(PackItem {
                path: doc.path.clone(),
                lang: doc.lang.clone(),
                name: sym.name.clone(),
                kind: sym.kind.clone(),
                line_start: sym.line_start,
                line_end: sym.line_end,
                signature: sym.signature.clone(),
                snippet,
                reason: c.reason.clone(),
                score: c.score,
            });
            if used >= budget_tokens {
                truncated = truncated || items.len() < cands.len();
                break;
            }
        }

        ContextPack {
            task: task.to_string(),
            budget_tokens,
            used_tokens: used,
            truncated,
            items,
        }
    }

    /// Blame a single line: the commit and author that last touched it.
    pub fn blame(&self, rel_path: &str, line: u32) -> Result<crate::git::BlameLine> {
        // Validate the path stays within the project root.
        self.resolve_within_root(rel_path)?;
        crate::git::blame(&self.paths.root, rel_path, line)
    }

    /// The commit history of a symbol: resolve `name` to its definition and list
    /// the commits that touched that line range, newest first.
    pub fn symbol_history(&self, name: &str, limit: usize) -> Result<SymbolHistory> {
        // Prefer the highest-ranked (non-test/vendor) definition.
        let defs = self.defs_by_name(name);
        let best = defs
            .iter()
            .max_by(|a, b| {
                let pa = self.segments[a.0]
                    .doc(a.2.doc_id)
                    .map(|d| path_score(&d.path))
                    .unwrap_or(0.0);
                let pb = self.segments[b.0]
                    .doc(b.2.doc_id)
                    .map(|d| path_score(&d.path))
                    .unwrap_or(0.0);
                pa.partial_cmp(&pb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .ok_or_else(|| Error::other(format!("no definition found for {name:?}")))?;
        let (si, _, sym) = *best;
        let doc = self.segments[si]
            .doc(sym.doc_id)
            .ok_or_else(|| Error::other("definition document missing".to_string()))?;
        let commits = crate::git::line_history(
            &self.paths.root,
            &doc.path,
            sym.line_start,
            sym.line_end,
            limit,
        )
        .or_else(|_| crate::git::file_history(&self.paths.root, &doc.path, limit))?;
        Ok(SymbolHistory {
            name: name.to_string(),
            path: doc.path.clone(),
            line_start: sym.line_start,
            line_end: sym.line_end,
            commits,
        })
    }

    /// Files changed since `rev`, annotated with the symbols defined in each
    /// (from the index) so an agent sees the affected API surface at a glance.
    pub fn changed_since(&self, rev: &str) -> Result<Vec<ChangedSymbols>> {
        let changed = crate::git::changed_since(&self.paths.root, rev)?;
        let mut out = Vec::with_capacity(changed.len());
        for cf in changed {
            let mut symbols = Vec::new();
            for seg in &self.segments {
                for (doc_id, doc) in seg.docs.iter().enumerate() {
                    if doc.path == cf.path && seg.is_live(doc_id as u32) {
                        for s in seg.doc_syms(doc_id as u32) {
                            symbols.push(s.name.clone());
                        }
                    }
                }
            }
            symbols.sort();
            symbols.dedup();
            out.push(ChangedSymbols {
                path: cf.path,
                status: cf.status,
                symbols,
            });
        }
        Ok(out)
    }

    /// Structural (AST) search: match a tree-sitter query or `$NAME`
    /// meta-variable pattern across documents of one language. Literal tokens in
    /// the pattern prune candidates via the trigram index before parsing.
    pub fn structural_search(
        &self,
        pattern: &str,
        lang: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<StructHit>> {
        let language = crate::lang::Language::from_id(lang)
            .ok_or_else(|| Error::other(format!("unknown language id: {lang:?}")))?;
        if language.grammar().is_none() {
            return Err(Error::other(format!(
                "language {lang} is not parseable for structural search"
            )));
        }
        let compiled = crate::structural::compile(language, pattern)?;

        // Prefilter on the most selective literal anchor, if any.
        let anchor = compiled.anchors.iter().max_by_key(|a| a.len()).cloned();
        let tq = anchor
            .as_ref()
            .map(|a| TrigramQuery::from_literal(a.as_bytes()));

        let mut targets: Vec<(usize, u32)> = Vec::new();
        for (si, seg) in self.segments.iter().enumerate() {
            let candidates = match &tq {
                Some(q) => seg.candidates(q)?,
                None => seg.all_live(),
            };
            for doc_id in candidates.iter() {
                if !seg.is_live(doc_id) {
                    continue;
                }
                match seg.doc(doc_id) {
                    Some(d) if d.lang == lang => targets.push((si, doc_id)),
                    _ => {}
                }
            }
        }

        let root = &self.paths.root;
        let segments = &self.segments;
        let content = &self.content;
        let compiled_ref = &compiled;
        let hits: Vec<StructHit> = targets
            .par_iter()
            .flat_map_iter(|&(si, doc_id)| {
                let seg = &segments[si];
                let doc = match seg.doc(doc_id) {
                    Some(d) => d,
                    None => return Vec::new().into_iter(),
                };
                let full = root.join(&doc.path);
                let data = match content.get_or_read(doc.hash, &full) {
                    Some(d) => d,
                    None => return Vec::new().into_iter(),
                };
                let matches = crate::structural::run(language, compiled_ref, &data);
                let line_starts = line_starts(&data);
                let out: Vec<StructHit> = matches
                    .into_iter()
                    .map(|m| {
                        let li = (m.line_start.saturating_sub(1)) as usize;
                        let text = line_starts
                            .get(li)
                            .map(|_| snippet(line_slice(&data, &line_starts, li)))
                            .unwrap_or_default();
                        StructHit {
                            path: doc.path.clone(),
                            lang: doc.lang.clone(),
                            line_start: m.line_start,
                            line_end: m.line_end,
                            kind: m.kind,
                            text,
                            captures: m.captures,
                        }
                    })
                    .collect();
                out.into_iter()
            })
            .collect();

        let cmp = |a: &StructHit, b: &StructHit| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.line_start.cmp(&b.line_start))
        };
        let mut hits = hits;
        hits.sort_by(cmp);
        Ok(paginate(hits, offset, limit))
    }

    /// Summarize the indexed repository.
    pub fn summary(&self) -> RepoSummary {
        use std::collections::HashMap;
        let mut by_lang: HashMap<String, LangStat> = HashMap::new();
        let mut by_dir: HashMap<String, u64> = HashMap::new();
        let mut files = 0u64;
        let mut bytes = 0u64;
        let mut symbols = 0u64;
        for seg in &self.segments {
            for (doc_id, doc) in seg.docs.iter().enumerate() {
                if !seg.is_live(doc_id as u32) {
                    continue;
                }
                files += 1;
                bytes += doc.size;
                let e = by_lang.entry(doc.lang.clone()).or_default();
                e.files += 1;
                e.bytes += doc.size;
                let dir = doc.path.split('/').next().unwrap_or("").to_string();
                *by_dir.entry(dir).or_default() += 1;
            }
            symbols += seg.syms.iter().filter(|s| seg.is_live(s.doc_id)).count() as u64;
        }
        let mut languages: Vec<LangStat> = by_lang
            .into_iter()
            .map(|(lang, mut s)| {
                s.lang = lang;
                s
            })
            .collect();
        languages.sort_by_key(|s| std::cmp::Reverse(s.files));
        let mut top_dirs: Vec<(String, u64)> = by_dir.into_iter().collect();
        top_dirs.sort_by_key(|d| std::cmp::Reverse(d.1));
        top_dirs.truncate(15);
        RepoSummary {
            files,
            bytes,
            symbols,
            segments: self.segments.len(),
            languages,
            top_dirs: top_dirs
                .into_iter()
                .map(|(name, files)| DirStat { name, files })
                .collect(),
        }
    }
}

/// A file slice with context, returned by [`Searcher::read_snippet`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snippet {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub total_lines: u32,
    pub lines: Vec<SnippetLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetLine {
    pub line: u32,
    pub text: String,
}

/// Repository summary returned by [`Searcher::summary`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSummary {
    pub files: u64,
    pub bytes: u64,
    pub symbols: u64,
    pub segments: usize,
    pub languages: Vec<LangStat>,
    pub top_dirs: Vec<DirStat>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LangStat {
    pub lang: String,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirStat {
    pub name: String,
    pub files: u64,
}

/// Read a single candidate file and collect matching lines. Matches are found
/// over the whole buffer (so regexes may span lines), mapped to line numbers,
/// then ranked so the highest-scored matches survive `max_per_file` truncation.
#[allow(clippy::too_many_arguments)] // hot path; threading a struct adds churn without clarity
fn verify_doc(
    seg: &Segment,
    doc_id: u32,
    root: &Path,
    content: &ContentCache,
    matcher: &Matcher,
    max_per_file: usize,
    whole_word: bool,
    exhaustive: bool,
) -> Vec<SearchHit> {
    let doc = match seg.doc(doc_id) {
        Some(d) => d,
        None => return Vec::new(),
    };
    let full = root.join(&doc.path);
    let data = match content.get_or_read(doc.hash, &full) {
        Some(d) => d,
        None => return Vec::new(),
    };

    // Exhaustive search lifts the pathological-input cap so no match is dropped.
    let cap = if exhaustive { usize::MAX } else { PER_FILE_MATCH_CAP };
    let matches = matcher.match_starts(&data, whole_word, cap);
    if matches.is_empty() {
        return Vec::new();
    }

    let line_starts = line_starts(&data);
    let sym_lines = symbol_lines(seg, doc_id);
    let base = path_score(&doc.path);

    let mut out = Vec::new();
    let mut last_line = 0u32;
    for (start, _end) in matches {
        let li = line_of(start, &line_starts);
        let line_no = li as u32 + 1;
        // One hit per line; matches are in ascending offset order.
        if line_no == last_line {
            continue;
        }
        last_line = line_no;
        let col = (start - line_starts[li]) as u32 + 1;
        let line_bytes = line_slice(&data, &line_starts, li);
        let mut score = 1.0 + base;
        if sym_lines.contains(&line_no) {
            score += 3.0;
        }
        out.push(SearchHit {
            path: doc.path.clone(),
            lang: doc.lang.clone(),
            line: line_no,
            column: col,
            text: snippet(line_bytes),
            score,
        });
    }

    // Keep the highest-scored matches when a file has more than the cap.
    // Exhaustive mode keeps every line.
    if !exhaustive && out.len() > max_per_file {
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.line.cmp(&b.line))
        });
        out.truncate(max_per_file);
    }
    out
}

/// Byte offsets at which each line begins (index 0 is the start of the file).
fn line_starts(data: &[u8]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(64);
    starts.push(0usize);
    for p in memchr::memchr_iter(b'\n', data) {
        starts.push(p + 1);
    }
    starts
}

/// Zero-based line index containing byte offset `off`.
fn line_of(off: usize, starts: &[usize]) -> usize {
    // Greatest line start that is <= off.
    starts.partition_point(|&s| s <= off).saturating_sub(1)
}

/// The bytes of line `li` (without the trailing newline).
fn line_slice<'a>(data: &'a [u8], starts: &[usize], li: usize) -> &'a [u8] {
    let begin = starts[li];
    let end = if li + 1 < starts.len() {
        starts[li + 1].saturating_sub(1)
    } else {
        data.len()
    };
    &data[begin..end.min(data.len())]
}

/// Ranking adjustment based on the file path: prefer shallow paths and
/// non-generated/non-test files.
fn path_score(path: &str) -> f32 {
    let mut s = 0.0f32;
    let depth = path.matches('/').count() as f32;
    s -= depth * 0.05;
    let lower = path.to_ascii_lowercase();
    if lower.contains("test")
        || lower.contains("/tests/")
        || lower.contains("__tests__")
        || lower.contains(".test.")
        || lower.contains(".spec.")
    {
        s -= 1.0;
    }
    if lower.contains("/vendor/") || lower.contains("/generated/") || lower.contains(".min.") {
        s -= 1.5;
    }
    s
}

fn symbol_lines(seg: &Segment, doc_id: u32) -> HashSet<u32> {
    seg.doc_syms(doc_id).map(|s| s.line_start).collect()
}

fn match_symbol(name: &str, lower: &str, needle: &str, exact: bool) -> Option<f32> {
    if exact {
        return if lower == needle { Some(100.0) } else { None };
    }
    if lower == needle {
        Some(100.0)
    } else if lower.starts_with(needle) {
        Some(70.0)
    } else if acronym(name) == needle {
        // e.g. "lc" matches loadConfig / load_config.
        Some(60.0)
    } else if lower.contains(needle) {
        Some(50.0)
    } else if is_subsequence(needle, lower) {
        Some(30.0)
    } else {
        None
    }
}

/// Split an identifier into lowercase tokens on camelCase and snake/kebab.
fn split_identifier(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in s.chars() {
        if ch == '_' || ch == '-' || ch == ' ' {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
        cur.extend(ch.to_lowercase());
        prev_lower = ch.is_lowercase() || ch.is_numeric();
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// First letter of each identifier token, lowercased.
fn acronym(s: &str) -> String {
    split_identifier(s)
        .iter()
        .filter_map(|t| t.chars().next())
        .collect()
}

/// Rank `items` best-first and apply offset/limit. Uses a partial selection so
/// we only fully sort the `offset + limit` items we actually return.
fn rank_paginate<T, F>(mut items: Vec<T>, cmp: F, offset: usize, limit: usize) -> Vec<T>
where
    F: Fn(&T, &T) -> std::cmp::Ordering,
{
    let need = offset.saturating_add(limit);
    if need == 0 {
        return Vec::new();
    }
    if need < items.len() {
        items.select_nth_unstable_by(need - 1, |a, b| cmp(a, b));
        items.truncate(need);
    }
    items.sort_by(|a, b| cmp(a, b));
    if offset >= items.len() {
        return Vec::new();
    }
    items.drain(0..offset);
    items.truncate(limit);
    items
}

/// Index-free fallback search: walk the working tree and scan every file with
/// the matcher, with no trigram prefilter. Used when the index is missing or
/// errors, so `search` still returns grep-equivalent results instead of failing.
/// Honors the same `lang`/`path` filters, `exhaustive` mode, and ordering as the
/// indexed path. Slower (reads every candidate file) but correct and complete.
pub fn grep_walk(paths: &Paths, config: &Config, query: &SearchQuery) -> Result<Vec<SearchHit>> {
    if query.pattern.is_empty() {
        return Ok(Vec::new());
    }
    let matcher = Matcher::build(query)?;
    let walked = crate::walk::walk(paths, config)?;
    let path_filter = query.path.as_deref();
    let lang_filter = query.lang.as_deref();
    let max_per_file = query.max_per_file;
    let whole_word = query.whole_word;
    let exhaustive = query.exhaustive;
    let index_binary = config.index_binary;
    let cap = if exhaustive { usize::MAX } else { PER_FILE_MATCH_CAP };

    let mut hits: Vec<SearchHit> = walked
        .entries
        .par_iter()
        .flat_map_iter(|e| {
            if path_filter.is_some_and(|pf| !e.rel.contains(pf)) {
                return Vec::new().into_iter();
            }
            let ext = e
                .path
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let lang_id = Language::from_extension(&ext).id().to_string();
            if lang_filter.is_some_and(|lf| lang_id != lf) {
                return Vec::new().into_iter();
            }
            let data = match std::fs::read(&e.path) {
                Ok(d) => d,
                Err(_) => return Vec::new().into_iter(),
            };
            if !index_binary && memchr::memchr(0, &data).is_some() {
                return Vec::new().into_iter();
            }
            let matches = matcher.match_starts(&data, whole_word, cap);
            if matches.is_empty() {
                return Vec::new().into_iter();
            }
            let starts = line_starts(&data);
            let base = path_score(&e.rel);
            let mut out = Vec::new();
            let mut last_line = 0u32;
            for (start, _end) in matches {
                let li = line_of(start, &starts);
                let line_no = li as u32 + 1;
                if line_no == last_line {
                    continue;
                }
                last_line = line_no;
                let col = (start - starts[li]) as u32 + 1;
                out.push(SearchHit {
                    path: e.rel.clone(),
                    lang: lang_id.clone(),
                    line: line_no,
                    column: col,
                    text: snippet(line_slice(&data, &starts, li)),
                    score: 1.0 + base,
                });
            }
            if !exhaustive && out.len() > max_per_file {
                out.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.line.cmp(&b.line))
                });
                out.truncate(max_per_file);
            }
            out.into_iter()
        })
        .collect();

    if exhaustive {
        hits.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.column.cmp(&b.column))
        });
        return Ok(hits);
    }
    let cmp = |a: &SearchHit, b: &SearchHit| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    };
    Ok(rank_paginate(hits, cmp, query.offset, query.limit))
}

/// Number of leading path components shared by two relative paths.
fn shared_prefix_len(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(x, y)| x == y)
        .count()
}

/// Apply offset/limit to an already-ordered vector.
fn paginate<T>(mut items: Vec<T>, offset: usize, limit: usize) -> Vec<T> {
    if offset >= items.len() {
        return Vec::new();
    }
    items.drain(0..offset);
    items.truncate(limit);
    items
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut chars = needle.chars();
    let mut cur = chars.next();
    for h in haystack.chars() {
        if let Some(c) = cur {
            if c == h {
                cur = chars.next();
            }
        } else {
            break;
        }
    }
    cur.is_none()
}

/// Trim and bound a matched line for display.
fn snippet(line: &[u8]) -> String {
    let s = String::from_utf8_lossy(line);
    let trimmed = s.trim_end();
    const MAX: usize = 320;
    if trimmed.len() > MAX {
        let mut end = MAX;
        while !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    } else {
        trimmed.to_string()
    }
}
