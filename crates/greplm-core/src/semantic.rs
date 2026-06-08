//! Optional hybrid semantic search layer (feature `semantic`).
//!
//! Chunks code by symbol (falling back to whole files) and ranks chunks against
//! a query with two complementary retrievers, fused and reranked the way modern
//! code-search engines do:
//!
//! 1. **BM25** over code-aware tokens (camelCase / snake_case split + light
//!    stemming) — strong for identifiers and API names.
//! 2. **Embeddings** — cosine similarity over a pluggable [`Embedder`]. The
//!    default [`HashEmbedder`] is dependency-free and fully offline; implement
//!    the trait with a real static code model (e.g. Model2Vec) for top quality.
//!
//! The two ranked lists are fused with weighted **Reciprocal Rank Fusion**
//! (adaptive: symbol-like queries lean lexical, natural-language queries stay
//! balanced), then reranked with code-aware signals: definition boosts,
//! name/stem overlap, file coherence, and noise penalties for test/vendor/
//! generated/minified files.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};
use crate::lang::Language;
use crate::paths::Paths;
use crate::segment::RawSymbol;
use crate::walk;
use crate::Greplm;

const FORMAT_VERSION: u32 = 5;
const RRF_K: f32 = 60.0;
const CANDIDATES_PER_RETRIEVER: usize = 200;

/// Maximum lines per code chunk before it is split into overlapping windows.
const MAX_CHUNK_LINES: usize = 50;
/// Overlap (in lines) between consecutive windows of a long chunk.
const CHUNK_OVERLAP: usize = 8;
/// Window size for non-code (symbol-less) files.
const MAX_FILE_CHUNK_LINES: usize = 80;
/// Minimum lines for a container's interstitial chunk to be worth emitting.
const MIN_CONTAINER_CHUNK_LINES: u32 = 3;
/// Cap on how many leading lines of a symbol-less file we chunk, to avoid
/// exploding the index on large data/generated files.
const MAX_FILE_LINES_TOTAL: usize = 800;

/// Produces a fixed-size embedding for a piece of text.
pub trait Embedder: Sync {
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Vec<f32>;
    /// Stable identifier for this embedder + its weights. Stored in the index so
    /// a search can refuse to run against vectors built by a different embedder.
    fn id(&self) -> String;
}

/// Offline feature-hashing embedder over code tokens (no model required).
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(16) }
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new(256)
    }
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn id(&self) -> String {
        format!("hash:{}", self.dim)
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        for token in tokenize(text) {
            let h = xxhash_rust::xxh3::xxh3_64(token.as_bytes());
            let idx = (h % self.dim as u64) as usize;
            let sign = if (h >> 63) & 1 == 1 { 1.0 } else { -1.0 };
            v[idx] += sign;
        }
        for x in v.iter_mut() {
            if *x != 0.0 {
                let s = x.signum();
                *x = s * (1.0 + x.abs().ln());
            }
        }
        l2_normalize(&mut v);
        v
    }
}

// ---------------------------------------------------------------------------
// Model2Vec embedder (trained static embeddings, e.g. potion-code-16M)
// ---------------------------------------------------------------------------

/// A trained Model2Vec static embedder. Loads a HuggingFace `tokenizer.json`
/// plus a `model.safetensors` containing `embeddings` (and optional SIF
/// `mapping`/`weights`). Encoding mirrors `model2vec`'s `StaticModel.encode`:
/// tokenize (no special tokens, drop `[UNK]`), remap ids, weight, mean-pool,
/// then L2-normalize when the model's config requests it.
#[cfg(feature = "semantic")]
pub struct Model2VecEmbedder {
    tokenizer: tokenizers::Tokenizer,
    emb: Vec<f32>,
    dim: usize,
    mapping: Option<Vec<i64>>,
    weights: Option<Vec<f64>>,
    normalize: bool,
    unk_id: Option<u32>,
    max_length: usize,
    char_cap: usize,
    id: String,
}

#[cfg(feature = "semantic")]
impl Model2VecEmbedder {
    /// Load a Model2Vec model from a directory containing `tokenizer.json`,
    /// `model.safetensors`, and (optionally) `config.json`.
    pub fn from_dir(dir: &Path) -> Result<Model2VecEmbedder> {
        let tok_path = dir.join("tokenizer.json");
        let tokenizer = tokenizers::Tokenizer::from_file(&tok_path)
            .map_err(|e| Error::other(format!("load tokenizer {}: {e}", tok_path.display())))?;

        let st_path = dir.join("model.safetensors");
        let bytes = std::fs::read(&st_path).map_err(|e| Error::io(&st_path, e))?;
        let tensors = parse_safetensors(&bytes)?;

        let (emb_shape, emb) = tensors
            .get("embeddings")
            .map(|t| (t.shape.clone(), tensor_f32(t)))
            .ok_or_else(|| Error::other("safetensors missing `embeddings` tensor"))?;
        let emb = emb?;
        if emb_shape.len() != 2 || emb_shape[1] == 0 {
            return Err(Error::other("embeddings tensor is not 2-D"));
        }
        let dim = emb_shape[1];

        let mapping = match tensors.get("mapping") {
            Some(t) => Some(tensor_i64(t)?),
            None => None,
        };
        let weights = match tensors.get("weights") {
            Some(t) => Some(tensor_f64(t)?),
            None => None,
        };

        let normalize = read_normalize(dir).unwrap_or(true);
        let unk_id = tokenizer.token_to_id("[UNK]");

        // Median token length, used (as model2vec does) to cap input chars
        // before tokenizing very long text.
        let median_token_len = median_token_len(&tokenizer).max(1);
        let max_length = 512usize;
        let char_cap = max_length * median_token_len;

        let id = format!("m2v:{}:{}:{}", dim, emb_shape[0], bytes.len());

        Ok(Model2VecEmbedder {
            tokenizer,
            emb,
            dim,
            mapping,
            weights,
            normalize,
            unk_id,
            max_length,
            char_cap,
            id,
        })
    }
}

#[cfg(feature = "semantic")]
impl Embedder for Model2VecEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn id(&self) -> String {
        self.id.clone()
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        // Cap input length (chars) the way model2vec does before tokenizing.
        let capped = if text.len() > self.char_cap {
            let mut end = self.char_cap;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            &text[..end]
        } else {
            text
        };

        let enc = match self.tokenizer.encode(capped, false) {
            Ok(e) => e,
            Err(_) => return vec![0.0; self.dim],
        };
        let mut ids: Vec<u32> = enc.get_ids().to_vec();
        if let Some(unk) = self.unk_id {
            ids.retain(|&i| i != unk);
        }
        ids.truncate(self.max_length);
        if ids.is_empty() {
            return vec![0.0; self.dim];
        }

        let rows = self.emb.len() / self.dim;
        let mut acc = vec![0f64; self.dim];
        for &id in &ids {
            let id = id as usize;
            let row = match &self.mapping {
                Some(m) => m.get(id).copied().unwrap_or(0).max(0) as usize,
                None => id,
            };
            if row >= rows {
                continue;
            }
            let w = match &self.weights {
                Some(ws) => ws.get(id).copied().unwrap_or(1.0),
                None => 1.0,
            };
            let base = row * self.dim;
            let v = &self.emb[base..base + self.dim];
            for (a, x) in acc.iter_mut().zip(v) {
                *a += (*x as f64) * w;
            }
        }

        let n = ids.len() as f64;
        let mut out: Vec<f32> = acc.iter().map(|x| (x / n) as f32).collect();
        if self.normalize {
            let norm = (out.iter().map(|x| x * x).sum::<f32>()).sqrt() + 1e-32;
            for x in out.iter_mut() {
                *x /= norm;
            }
        }
        out
    }
}

/// Read the `normalize` flag from a Model2Vec `config.json`, if present.
#[cfg(feature = "semantic")]
fn read_normalize(dir: &Path) -> Option<bool> {
    let cfg = std::fs::read(dir.join("config.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&cfg).ok()?;
    v.get("normalize").and_then(|n| n.as_bool())
}

/// Median token string length over the tokenizer vocabulary.
#[cfg(feature = "semantic")]
fn median_token_len(tokenizer: &tokenizers::Tokenizer) -> usize {
    let vocab = tokenizer.get_vocab(true);
    if vocab.is_empty() {
        return 7;
    }
    let mut lens: Vec<usize> = vocab.keys().map(|t| t.chars().count()).collect();
    lens.sort_unstable();
    lens[lens.len() / 2]
}

// ---- Minimal safetensors reader ------------------------------------------

#[cfg(feature = "semantic")]
struct RawTensor<'a> {
    dtype: String,
    shape: Vec<usize>,
    data: &'a [u8],
}

/// Parse a safetensors blob into a map of tensor name -> raw bytes + metadata.
#[cfg(feature = "semantic")]
fn parse_safetensors(bytes: &[u8]) -> Result<std::collections::HashMap<String, RawTensor<'_>>> {
    if bytes.len() < 8 {
        return Err(Error::Corrupt("safetensors: too short".into()));
    }
    let n = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize;
    let header_end = 8 + n;
    if header_end > bytes.len() {
        return Err(Error::Corrupt("safetensors: header out of range".into()));
    }
    let header: serde_json::Value = serde_json::from_slice(&bytes[8..header_end])
        .map_err(|e| Error::Corrupt(format!("safetensors header: {e}")))?;
    let obj = header
        .as_object()
        .ok_or_else(|| Error::Corrupt("safetensors header not an object".into()))?;
    let data = &bytes[header_end..];
    let mut out = std::collections::HashMap::new();
    for (name, meta) in obj {
        if name == "__metadata__" {
            continue;
        }
        let dtype = meta
            .get("dtype")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        let shape: Vec<usize> = meta
            .get("shape")
            .and_then(|s| s.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_u64().map(|v| v as usize))
                    .collect()
            })
            .unwrap_or_default();
        let offs = meta
            .get("data_offsets")
            .and_then(|o| o.as_array())
            .ok_or_else(|| Error::Corrupt("safetensors: missing data_offsets".into()))?;
        let start = offs.first().and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let end = offs.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        if end > data.len() || start > end {
            return Err(Error::Corrupt(format!(
                "safetensors: tensor {name} out of range"
            )));
        }
        out.insert(
            name.clone(),
            RawTensor {
                dtype,
                shape,
                data: &data[start..end],
            },
        );
    }
    Ok(out)
}

#[cfg(feature = "semantic")]
fn tensor_f32(t: &RawTensor<'_>) -> Result<Vec<f32>> {
    match t.dtype.as_str() {
        "F32" => Ok(t
            .data
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()),
        "F64" => Ok(t
            .data
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()) as f32)
            .collect()),
        "F16" => Ok(t
            .data
            .chunks_exact(2)
            .map(|c| f16_to_f32(u16::from_le_bytes([c[0], c[1]])))
            .collect()),
        other => Err(Error::other(format!(
            "unsupported embeddings dtype {other}"
        ))),
    }
}

#[cfg(feature = "semantic")]
fn tensor_i64(t: &RawTensor<'_>) -> Result<Vec<i64>> {
    if t.dtype != "I64" {
        return Err(Error::other(format!(
            "expected I64 mapping, got {}",
            t.dtype
        )));
    }
    Ok(t.data
        .chunks_exact(8)
        .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
        .collect())
}

#[cfg(feature = "semantic")]
fn tensor_f64(t: &RawTensor<'_>) -> Result<Vec<f64>> {
    match t.dtype.as_str() {
        "F64" => Ok(t
            .data
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect()),
        "F32" => Ok(t
            .data
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64)
            .collect()),
        other => Err(Error::other(format!("unsupported weights dtype {other}"))),
    }
}

/// Convert an IEEE-754 half-precision value to f32.
#[cfg(feature = "semantic")]
fn f16_to_f32(h: u16) -> f32 {
    let sign = (h >> 15) & 1;
    let exp = (h >> 10) & 0x1f;
    let frac = h & 0x3ff;
    let val = match exp {
        0 => (frac as f32) * 2f32.powi(-24),
        0x1f => {
            if frac == 0 {
                f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => (1.0 + frac as f32 / 1024.0) * 2f32.powi(exp as i32 - 15),
    };
    if sign == 1 {
        -val
    } else {
        val
    }
}

// ---------------------------------------------------------------------------
// Tokenization
// ---------------------------------------------------------------------------

/// Tokenize text into lowercase identifier sub-tokens (camel/snake aware),
/// including the whole word for exact-token signal.
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            word.push(ch);
        } else if !word.is_empty() {
            push_subtokens(&word, &mut tokens);
            word.clear();
        }
    }
    if !word.is_empty() {
        push_subtokens(&word, &mut tokens);
    }
    tokens
}

fn push_subtokens(word: &str, out: &mut Vec<String>) {
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in word.chars() {
        if ch == '_' {
            if cur.len() >= 2 {
                out.push(cur.to_lowercase());
            }
            cur.clear();
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !cur.is_empty() {
            if cur.len() >= 2 {
                out.push(cur.to_lowercase());
            }
            cur.clear();
        }
        cur.push(ch);
        prev_lower = ch.is_lowercase() || ch.is_numeric();
    }
    if cur.len() >= 2 {
        out.push(cur.to_lowercase());
    }
    if word.len() >= 2 {
        out.push(word.to_lowercase());
    }
}

/// Very light suffix stemmer so `parsing`/`parsed`/`parses` collapse to `pars`.
fn stem(token: &str) -> String {
    let t = token;
    let n = t.len();
    for (suf, keep) in [("ing", 4), ("ed", 3), ("es", 3), ("s", 4)] {
        if n >= keep && t.ends_with(suf) {
            return t[..n - suf.len()].to_string();
        }
    }
    t.to_string()
}

fn stemmed_tokens(text: &str) -> Vec<String> {
    tokenize(text).iter().map(|t| stem(t)).collect()
}

fn term_hash(s: &str) -> u64 {
    xxhash_rust::xxh3::xxh3_64(s.as_bytes())
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Structural path segments that carry no semantic intent across essentially
/// all ecosystems (build/layout scaffolding, not feature names). Dropping them
/// keeps the path signal about the *feature directory* and *file*. This list is
/// language-agnostic on purpose — it must not encode any one project's layout.
const STRUCTURAL_DIRS: &[&str] = &[
    // generic source/layout wrappers
    "src",
    "lib",
    "libs",
    "pkg",
    "app",
    "apps",
    "core",
    "internal",
    "packages",
    "common",
    "components",
    "component",
    "modules",
    "module",
    "utils",
    "util",
    "helpers",
    "helper",
    "shared",
    "main",
    "include",
    "includes",
    // tests / fixtures / examples / generated
    "test",
    "tests",
    "spec",
    "specs",
    "__tests__",
    "fixtures",
    "examples",
    "example",
    "demo",
    "mocks",
    "__mocks__",
    "migrations",
    "generated",
    "gen",
    "dist",
    "build",
    "target",
    "out",
    "node_modules",
    "vendor",
    // common framework sub-layers (kept generic; present in many stacks)
    "models",
    "model",
    "views",
    "view",
    "controllers",
    "controller",
    "services",
    "service",
    "handlers",
    "handler",
    "routes",
    "router",
    "middleware",
    "middlewares",
    "data",
    "static",
    "assets",
    "public",
    "security",
    "report",
    "reports",
    "wizard",
    "wizards",
    "i18n",
    "locale",
    "locales",
];

/// Humanize a repo-relative path into a bag of intent tokens: feature
/// directories + file stem, with structural scaffolding dirs dropped and
/// camel/snake boundaries split. Works for any layout, e.g.
/// `packages/core/src/middleware/flip.ts` -> "flip" and
/// `wms/stock_release_channel_cutoff/models/stock_release_channel.py`
/// -> "stock release channel cutoff stock release channel".
///
/// We do not special-case the repo root or depth: structural-dir filtering
/// plus BM25's idf naturally suppress high-frequency wrapper segments, so the
/// signal generalizes instead of being tuned to one project's nesting.
fn path_tokens(rel: &str) -> Vec<String> {
    let no_ext = rel.rsplit_once('.').map(|(a, _)| a).unwrap_or(rel);
    let segs: Vec<&str> = no_ext.split('/').collect();
    let mut out = Vec::new();
    for (idx, seg) in segs.iter().enumerate() {
        let is_last = idx + 1 == segs.len();
        // Drop scaffolding dirs, but never the filename itself.
        if !is_last && STRUCTURAL_DIRS.contains(&seg.to_ascii_lowercase().as_str()) {
            continue;
        }
        // A bare `index`/`mod`/`__init__` filename carries no intent; the
        // meaningful name is its parent directory, already captured above.
        if is_last
            && matches!(
                seg.to_ascii_lowercase().as_str(),
                "index" | "mod" | "__init__" | "main" | "lib"
            )
        {
            continue;
        }
        push_subtokens(seg, &mut out);
    }
    out
}

/// A query is "symbol-like" when it has no spaces and looks like an identifier
/// path (snake_case, camelCase, `Foo::bar`, `_private`, etc.).
fn is_symbol_like(query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() || q.contains(char::is_whitespace) {
        return false;
    }
    q.contains('_')
        || q.contains("::")
        || q.contains('.')
        || q.chars().any(|c| c.is_uppercase()) && q.chars().any(|c| c.is_lowercase())
}

// ---------------------------------------------------------------------------
// On-disk structures
// ---------------------------------------------------------------------------

/// Metadata for a single embedded chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkMeta {
    pub path: String,
    pub lang: String,
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    version: u32,
    dim: usize,
    avgdl: f32,
    /// Identifier of the embedder that produced the vectors (see
    /// [`Embedder::id`]). A search refuses to run with a different embedder.
    #[serde(default)]
    model_id: String,
    chunks: Vec<ChunkMeta>,
}

/// A semantic search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticHit {
    pub path: String,
    pub lang: String,
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    pub score: f32,
}

fn meta_path(p: &Paths) -> std::path::PathBuf {
    p.base.join("semantic.meta.json")
}
fn vecs_path(p: &Paths) -> std::path::PathBuf {
    p.base.join("semantic.vecs.bin")
}
fn bm25_path(p: &Paths) -> std::path::PathBuf {
    p.base.join("semantic.bm25.bin")
}

// ---------------------------------------------------------------------------
// Chunking
// ---------------------------------------------------------------------------

/// A planned chunk: a line range plus the symbol metadata to attach to it.
struct ChunkSpec {
    name: String,
    kind: String,
    line_start: u32,
    line_end: u32,
    sig: Option<String>,
}

/// Build method-level chunks from a file's symbols. A container symbol (class,
/// module, etc.) keeps only the lines it *owns* — its declaration and any
/// attributes between members — while nested symbols (methods, inner functions)
/// become their own chunks. This prevents a class chunk from swallowing every
/// method's text and out-ranking the methods themselves. Long bodies are split
/// into overlapping windows so no single chunk is unbounded.
fn symbol_chunk_specs(symbols: &[RawSymbol]) -> Vec<ChunkSpec> {
    let mut specs = Vec::new();
    for (i, s) in symbols.iter().enumerate() {
        let s_start = s.line_start;
        let s_end = s.line_end.max(s.line_start);
        let intervals = own_intervals(i, symbols);
        // A leaf symbol (no nested children) owns its whole range as one
        // interval; keep it even if short. A container's own lines are the gaps
        // between its members — drop the tiny interstitial fragments (blank
        // lines, single attributes) so we don't emit useless 1-line chunks.
        let is_leaf = intervals.len() == 1 && intervals[0] == (s_start, s_end);
        for (a, b) in intervals {
            if !is_leaf && (b - a + 1) < MIN_CONTAINER_CHUNK_LINES {
                continue;
            }
            window_into(&mut specs, s, a, b);
        }
    }
    specs
}

/// Split `[a, b]` (inclusive, 1-based lines) into overlapping windows of at most
/// `MAX_CHUNK_LINES`, pushing a [`ChunkSpec`] for each. Only the first window of
/// a symbol carries its signature.
fn window_into(specs: &mut Vec<ChunkSpec>, s: &RawSymbol, a: u32, b: u32) {
    let mut start = a;
    loop {
        let end = ((start as usize + MAX_CHUNK_LINES - 1) as u32).min(b);
        specs.push(ChunkSpec {
            name: s.name.clone(),
            kind: s.kind.clone(),
            line_start: start,
            line_end: end,
            sig: if start == a {
                s.signature.clone()
            } else {
                None
            },
        });
        if end >= b {
            break;
        }
        start = end + 1 - CHUNK_OVERLAP as u32;
    }
}

/// The line intervals symbol `i` owns: its own range minus the ranges of any
/// other symbols strictly nested inside it.
fn own_intervals(i: usize, symbols: &[RawSymbol]) -> Vec<(u32, u32)> {
    let s = &symbols[i];
    let s_start = s.line_start;
    let s_end = s.line_end.max(s.line_start);
    let mut children: Vec<(u32, u32)> = Vec::new();
    for (j, o) in symbols.iter().enumerate() {
        if j == i {
            continue;
        }
        let o_end = o.line_end.max(o.line_start);
        let inside = o.line_start >= s_start && o_end <= s_end;
        let proper = (o.line_start, o_end) != (s_start, s_end)
            && o_end.saturating_sub(o.line_start) < s_end.saturating_sub(s_start);
        if inside && proper {
            children.push((o.line_start, o_end));
        }
    }
    subtract_intervals(s_start, s_end, &mut children)
}

/// Return `[start, end]` with the (possibly overlapping) `holes` removed.
fn subtract_intervals(start: u32, end: u32, holes: &mut [(u32, u32)]) -> Vec<(u32, u32)> {
    holes.sort_unstable_by_key(|h| h.0);
    let mut out = Vec::new();
    let mut cur = start;
    for &(hs, he) in holes.iter() {
        let hs = hs.max(start);
        let he = he.min(end);
        if hs > he {
            continue;
        }
        if hs > cur {
            out.push((cur, hs - 1));
        }
        cur = cur.max(he + 1);
        if cur > end {
            break;
        }
    }
    if cur <= end {
        out.push((cur, end));
    }
    out
}

/// Window a symbol-less file's leading lines into fixed-size chunks.
fn file_chunk_specs(rel: &str, total_lines: usize) -> Vec<ChunkSpec> {
    let n = total_lines.min(MAX_FILE_LINES_TOTAL);
    if n == 0 {
        return Vec::new();
    }
    let name = file_stem(rel);
    let mut specs = Vec::new();
    let mut start = 1u32;
    loop {
        let end = ((start as usize + MAX_FILE_CHUNK_LINES - 1).min(n)) as u32;
        specs.push(ChunkSpec {
            name: name.clone(),
            kind: "file".to_string(),
            line_start: start,
            line_end: end,
            sig: None,
        });
        if end as usize >= n {
            break;
        }
        start = end + 1 - CHUNK_OVERLAP as u32;
    }
    specs
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// All chunks produced from a single file: aligned metadata, embeddings (flat,
/// `dim` floats each), and BM25 postings, plus the file's total BM25 token length.
struct FileChunks {
    chunks: Vec<ChunkMeta>,
    vecs: Vec<f32>,
    postings: Vec<Vec<(u64, u32)>>,
    total_len: u64,
}

/// Chunk and embed a single file. Returns `None` for unreadable, binary, or
/// non-UTF-8 files. Pure with respect to shared state, so it is safe to run
/// across rayon worker threads.
fn chunk_file(entry: &walk::WalkEntry, embedder: &dyn Embedder) -> Option<FileChunks> {
    let data = std::fs::read(&entry.path).ok()?;
    if memchr::memchr(0, &data).is_some() {
        return None;
    }
    let text = String::from_utf8(data).ok()?;
    let ext = entry
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let lang = Language::from_extension(&ext);
    let lines: Vec<&str> = text.lines().collect();
    let symbols = if lang.grammar().is_some() {
        crate::symbol::extract(lang, text.as_bytes())
    } else {
        Vec::new()
    };

    // Intent tokens from the module path + filename. In real repos the
    // module/file name is a strong, low-noise statement of what the code is
    // about ("stock_demand_estimate"); fold it into both retrievers.
    let p_text = path_tokens(&entry.rel).join(" ");

    let mut out = FileChunks {
        chunks: Vec::new(),
        vecs: Vec::new(),
        postings: Vec::new(),
        total_len: 0,
    };

    let mut emit = |meta: ChunkMeta, body: &str, name: &str, sig: Option<&str>| {
        // BM25 tokens: path + name + signature weighted by repetition, plus body.
        let mut bm_text = String::new();
        for _ in 0..2 {
            bm_text.push_str(&p_text);
            bm_text.push(' ');
        }
        for _ in 0..3 {
            bm_text.push_str(name);
            bm_text.push(' ');
        }
        if let Some(s) = sig {
            bm_text.push_str(s);
            bm_text.push(' ');
        }
        bm_text.push_str(body);
        let mut counts: HashMap<u64, u32> = HashMap::new();
        for tok in stemmed_tokens(&bm_text) {
            *counts.entry(term_hash(&tok)).or_insert(0) += 1;
        }
        let dl: u64 = counts.values().map(|v| *v as u64).sum();
        out.total_len += dl;
        out.postings.push(counts.into_iter().collect());

        // Embedding text: path + name + signature + body.
        let mut emb_text = String::new();
        emb_text.push_str(&p_text);
        emb_text.push(' ');
        emb_text.push_str(name);
        emb_text.push(' ');
        if let Some(s) = sig {
            emb_text.push_str(s);
            emb_text.push(' ');
        }
        emb_text.push_str(body);
        out.vecs.extend_from_slice(&embedder.embed(&emb_text));
        out.chunks.push(meta);
    };

    let specs = if symbols.is_empty() {
        file_chunk_specs(&entry.rel, lines.len())
    } else {
        symbol_chunk_specs(&symbols)
    };
    for spec in specs {
        let from = spec.line_start.saturating_sub(1) as usize;
        let to = (spec.line_end as usize).min(lines.len());
        if from >= to {
            continue;
        }
        let body = lines[from..to].join("\n");
        emit(
            ChunkMeta {
                path: entry.rel.clone(),
                lang: lang.id().to_string(),
                name: spec.name.clone(),
                kind: spec.kind.clone(),
                line_start: spec.line_start,
                line_end: spec.line_end,
            },
            &body,
            &spec.name,
            spec.sig.as_deref(),
        );
    }
    Some(out)
}

/// Build the hybrid semantic index for a project.
pub fn build(greplm: &Greplm, embedder: &dyn Embedder) -> Result<usize> {
    greplm.ensure_initialized()?;
    let paths = Paths::new(greplm.root());
    let config = greplm.config().clone();
    let entries = walk::walk(&paths, &config)?;

    let dim = embedder.dim();

    // Chunk + embed each file in parallel. Embedding is the dominant cost, and
    // rayon's `collect` preserves input order, so the concatenated
    // chunks/vecs/postings stay aligned and the output is deterministic.
    let per_file: Vec<FileChunks> = entries
        .par_iter()
        .filter_map(|entry| chunk_file(entry, embedder))
        .collect();

    let mut chunks: Vec<ChunkMeta> = Vec::new();
    let mut vecs: Vec<f32> = Vec::new();
    // Per-chunk BM25 postings: (term_hash, tf).
    let mut postings: Vec<Vec<(u64, u32)>> = Vec::new();
    let mut total_len: u64 = 0;
    for fc in per_file {
        chunks.extend(fc.chunks);
        vecs.extend(fc.vecs);
        postings.extend(fc.postings);
        total_len += fc.total_len;
    }

    let count = chunks.len();
    let avgdl = if count > 0 {
        total_len as f32 / count as f32
    } else {
        0.0
    };

    let header = Header {
        version: FORMAT_VERSION,
        dim,
        avgdl,
        model_id: embedder.id(),
        chunks,
    };
    std::fs::write(meta_path(&paths), serde_json::to_vec(&header)?)
        .map_err(|e| Error::io(meta_path(&paths), e))?;

    let mut vbytes = Vec::with_capacity(vecs.len() * 4);
    for f in &vecs {
        vbytes.extend_from_slice(&f.to_le_bytes());
    }
    std::fs::write(vecs_path(&paths), vbytes).map_err(|e| Error::io(vecs_path(&paths), e))?;

    write_postings(&bm25_path(&paths), &postings)?;
    Ok(count)
}

fn write_postings(path: &Path, postings: &[Vec<(u64, u32)>]) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    for chunk in postings {
        buf.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        for (h, tf) in chunk {
            buf.extend_from_slice(&h.to_le_bytes());
            buf.extend_from_slice(&tf.to_le_bytes());
        }
    }
    let mut f = std::fs::File::create(path).map_err(|e| Error::io(path, e))?;
    f.write_all(&buf).map_err(|e| Error::io(path, e))?;
    Ok(())
}

fn read_postings(path: &Path, n_chunks: usize) -> Result<Vec<Vec<(u64, u32)>>> {
    let data = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    let mut out = Vec::with_capacity(n_chunks);
    let mut off = 0usize;
    let rd_u32 = |d: &[u8], o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
    for _ in 0..n_chunks {
        if off + 4 > data.len() {
            return Err(Error::Corrupt("bm25 postings truncated".into()));
        }
        let n = rd_u32(&data, off) as usize;
        off += 4;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            if off + 12 > data.len() {
                return Err(Error::Corrupt("bm25 postings truncated".into()));
            }
            let h = u64::from_le_bytes([
                data[off],
                data[off + 1],
                data[off + 2],
                data[off + 3],
                data[off + 4],
                data[off + 5],
                data[off + 6],
                data[off + 7],
            ]);
            let tf = rd_u32(&data, off + 8);
            off += 12;
            v.push((h, tf));
        }
        out.push(v);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Search the hybrid index for the top-`k` chunks most relevant to `query`.
pub fn search(
    greplm: &Greplm,
    embedder: &dyn Embedder,
    query: &str,
    k: usize,
) -> Result<Vec<SemanticHit>> {
    let paths = Paths::new(greplm.root());
    let meta_p = meta_path(&paths);
    if !meta_p.exists() {
        return Err(Error::other(
            "no semantic index; run `greplm semantic-index` first",
        ));
    }
    let header: Header =
        serde_json::from_slice(&std::fs::read(&meta_p).map_err(|e| Error::io(&meta_p, e))?)?;
    if header.version != FORMAT_VERSION {
        return Err(Error::other(
            "semantic index is from an older greplm; re-run `greplm semantic-index`",
        ));
    }
    if header.model_id != embedder.id() {
        return Err(Error::other(format!(
            "semantic index was built with embedder `{}` but search is using `{}`; \
             re-run `greplm semantic-index` with the same model",
            header.model_id,
            embedder.id()
        )));
    }
    let n = header.chunks.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let dim = header.dim;
    let raw = std::fs::read(vecs_path(&paths)).map_err(|e| Error::io(vecs_path(&paths), e))?;
    let floats = bytes_to_f32(&raw);
    if dim == 0 || floats.len() != n * dim {
        return Err(Error::Corrupt("semantic vectors size mismatch".into()));
    }
    let postings = read_postings(&bm25_path(&paths), n)?;

    // ---- BM25 retriever -------------------------------------------------
    let q_terms: Vec<u64> = {
        let mut seen = std::collections::HashSet::new();
        stemmed_tokens(query)
            .into_iter()
            .map(|t| term_hash(&t))
            .filter(|h| seen.insert(*h))
            .collect()
    };
    // Document frequency for query terms only.
    let mut df: HashMap<u64, u32> = HashMap::new();
    for qt in &q_terms {
        df.insert(*qt, 0);
    }
    for chunk in &postings {
        for (h, _) in chunk {
            if let Some(c) = df.get_mut(h) {
                *c += 1;
            }
        }
    }
    const K1: f32 = 1.2;
    const B: f32 = 0.75;
    let avgdl = header.avgdl.max(1.0);
    let mut bm25: Vec<(f32, usize)> = postings
        .par_iter()
        .enumerate()
        .filter_map(|(i, chunk)| {
            let dl: u32 = chunk.iter().map(|(_, tf)| *tf).sum();
            let mut score = 0.0f32;
            for (h, tf) in chunk {
                if let Some(&dfi) = df.get(h) {
                    if dfi == 0 {
                        continue;
                    }
                    let idf = (((n as f32 - dfi as f32 + 0.5) / (dfi as f32 + 0.5)) + 1.0).ln();
                    let tf = *tf as f32;
                    let denom = tf + K1 * (1.0 - B + B * dl as f32 / avgdl);
                    score += idf * (tf * (K1 + 1.0)) / denom;
                }
            }
            (score > 0.0).then_some((score, i))
        })
        .collect();
    bm25.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    bm25.truncate(CANDIDATES_PER_RETRIEVER);

    // ---- Embedding retriever -------------------------------------------
    let qvec = embedder.embed(query);
    let mut emb: Vec<(f32, usize)> = if qvec.len() == dim {
        (0..n)
            .into_par_iter()
            .map(|i| {
                let v = &floats[i * dim..(i + 1) * dim];
                (dot(&qvec, v), i)
            })
            .collect()
    } else {
        Vec::new()
    };
    emb.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    emb.truncate(CANDIDATES_PER_RETRIEVER);

    // ---- Weighted RRF fusion -------------------------------------------
    // Symbol-like queries lean lexical; natural-language queries lean on the
    // (now trained) embedder. Weights are env-tunable for offline sweeps.
    let symbolic = is_symbol_like(query);
    let (w_bm25, w_emb) = if symbolic {
        (
            env_f32("GREPLM_SEM_WBM25_SYM", 1.0),
            env_f32("GREPLM_SEM_WEMB_SYM", 0.35),
        )
    } else {
        // The embedder is a trained static code model; for natural-language
        // queries lean on it while BM25 still defines the candidate pool and
        // breaks ties. The weight was chosen for robustness across multiple
        // dissimilar repos (verbose snake_case modules and short camelCase TS
        // libraries), not the argmax on any single one.
        (
            env_f32("GREPLM_SEM_WBM25", 1.0),
            env_f32("GREPLM_SEM_WEMB", 2.5),
        )
    };
    let mut fused: HashMap<usize, f32> = HashMap::new();
    for (rank, (_, i)) in bm25.iter().enumerate() {
        *fused.entry(*i).or_insert(0.0) += w_bm25 / (RRF_K + rank as f32 + 1.0);
    }
    for (rank, (_, i)) in emb.iter().enumerate() {
        *fused.entry(*i).or_insert(0.0) += w_emb / (RRF_K + rank as f32 + 1.0);
    }

    // ---- Code-aware reranking ------------------------------------------
    // File coherence: how many candidate chunks share each file.
    let mut file_hits: HashMap<&str, u32> = HashMap::new();
    for i in fused.keys() {
        *file_hits
            .entry(header.chunks[*i].path.as_str())
            .or_insert(0) += 1;
    }
    let q_stem_set: std::collections::HashSet<String> = stemmed_tokens(query).into_iter().collect();
    let q_lower = query.to_ascii_lowercase();
    // Scaling knobs for the rerank signals (env-tunable for sweeps). Kept
    // small: path/name intent is already folded into both retrievers, so this
    // is just a tie-breaking nudge.
    let path_w = env_f32("GREPLM_SEM_PATHW", 0.012);
    let name_w = env_f32("GREPLM_SEM_NAMEW", 0.004);

    let mut scored: Vec<(f32, usize)> = fused
        .into_iter()
        .map(|(i, base)| {
            let c = &header.chunks[i];
            let mut s = base;

            // Definition boost: chunk whose symbol the query names.
            let name_lower = c.name.to_ascii_lowercase();
            if is_definition_kind(&c.kind) {
                if name_lower == q_lower {
                    s += 0.030;
                } else if q_lower.contains(&name_lower) || name_lower.contains(&q_lower) {
                    s += 0.012;
                }
            }
            // Name/stem overlap with the query.
            let name_stems: std::collections::HashSet<String> =
                stemmed_tokens(&c.name).into_iter().collect();
            let overlap = q_stem_set.intersection(&name_stems).count();
            if overlap > 0 {
                s += name_w * overlap as f32;
            }
            // Path/module intent overlap: the strongest non-body signal in real
            // repos, where the module name states what the code is for. Scaled
            // by the fraction of query terms the path covers (capped).
            let path_stems: std::collections::HashSet<String> =
                stemmed_tokens(&path_tokens(&c.path).join(" "))
                    .into_iter()
                    .collect();
            let p_overlap = q_stem_set.intersection(&path_stems).count();
            if p_overlap > 0 && !q_stem_set.is_empty() {
                let frac = p_overlap as f32 / q_stem_set.len() as f32;
                s += path_w * (p_overlap as f32) * (0.5 + frac);
            }
            // File coherence.
            if let Some(&cnt) = file_hits.get(c.path.as_str()) {
                if cnt > 1 {
                    s += 0.004 * (cnt as f32).ln();
                }
            }
            // Noise penalties.
            s -= noise_penalty(&c.path);

            (s, i)
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    // Collapse to the best chunk per file so the top-k isn't wasted on several
    // windows of one file (diverse files = better retrieval and UX).
    if env_f32("GREPLM_SEM_DEDUP_FILE", 1.0) > 0.0 {
        let mut seen = std::collections::HashSet::new();
        scored.retain(|(_, i)| seen.insert(header.chunks[*i].path.as_str()));
    }
    scored.truncate(k);

    Ok(scored
        .into_iter()
        .map(|(score, i)| {
            let c = &header.chunks[i];
            SemanticHit {
                path: c.path.clone(),
                lang: c.lang.clone(),
                name: c.name.clone(),
                kind: c.kind.clone(),
                line_start: c.line_start,
                line_end: c.line_end,
                score,
            }
        })
        .collect())
}

fn is_definition_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function"
            | "method"
            | "class"
            | "struct"
            | "trait"
            | "interface"
            | "enum"
            | "type"
            | "module"
            | "constructor"
    )
}

/// Down-rank test / generated / vendored / minified / declaration files.
fn noise_penalty(path: &str) -> f32 {
    let p = path.to_ascii_lowercase();
    let mut s = 0.0f32;
    if p.contains("/test")
        || p.contains("test/")
        || p.contains("/tests/")
        || p.contains("__tests__")
        || p.contains(".test.")
        || p.contains(".spec.")
        || p.contains("/spec/")
    {
        s += 0.040;
    }
    if p.contains("/legacy/") || p.contains("/compat/") || p.contains("/examples/") {
        s += 0.012;
    }
    if p.contains(".min.") || p.ends_with(".d.ts") || p.contains("/vendor/") || p.contains("/lib/")
    {
        s += 0.016;
    }
    if p.contains("/static/") || p.ends_with(".css") {
        s += 0.010;
    }
    // Translation catalogs and i18n data are essentially never the implementation
    // an agent wants; they match heavily on domain words ("stock", "move").
    if p.ends_with(".pot") || p.ends_with(".po") || p.contains("/i18n/") {
        s += 0.045;
    }
    // Prose docs (README/DESCRIPTION/USAGE/...). They describe a feature in
    // natural language, so they rank highly on NL queries and crowd out the
    // actual code. For *code* search they should sit below real source.
    if p.ends_with(".rst")
        || p.ends_with(".md")
        || p.ends_with(".mdx")
        || p.ends_with(".markdown")
        || p.ends_with(".mdown")
        || p.ends_with(".rdoc")
        || p.ends_with(".adoc")
        || p.ends_with(".txt")
        || p.contains("/readme")
        || p.contains("/docs/")
        || p.contains("/doc/")
        || p.contains("/website/")
        || p.contains("/documentation/")
    {
        s += 0.050;
    }
    // Markup/data/config: views, fixtures, manifests. Relevant sometimes, but
    // down-rank below real code for logic queries.
    if p.ends_with(".xml")
        || p.ends_with(".csv")
        || p.ends_with(".cfg")
        || p.ends_with(".ini")
        || p.ends_with(".toml")
        || p.ends_with(".yaml")
        || p.ends_with(".yml")
    {
        s += 0.022;
    }
    // Legal / changelog boilerplate matches common words ("rights", "use") but
    // is never the implementation an agent wants.
    let file = p.rsplit('/').next().unwrap_or(&p);
    if file.starts_with("license")
        || file.starts_with("copying")
        || file.starts_with("notice")
        || file.starts_with("authors")
        || file.starts_with("changelog")
    {
        s += 0.030;
    }
    s
}

/// Read an `f32` tuning knob from the environment, falling back to `default`.
/// Lets offline weight sweeps run without recompiling.
fn env_f32(key: &str, default: f32) -> f32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(default)
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn file_stem(rel: &str) -> String {
    Path::new(rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel)
        .to_string()
}
