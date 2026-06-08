//! Task-driven context packs.
//!
//! Given a natural-language task and a token budget, greplm assembles the most
//! relevant slice of the codebase an agent needs to act — ranked by lexical
//! relevance, call-graph centrality, and (when built with the `semantic`
//! feature) meaning — and packs it to fit the budget. This is the "give me
//! exactly the code for this task" surface that keeps agents off the
//! grep-then-read-whole-files treadmill.
//!
//! The ranking helpers here are pure; [`crate::search::Searcher::context_pack`]
//! drives them over the index.

use serde::{Deserialize, Serialize};

/// One unit of packed context: a symbol, its signature, and a code snippet.
///
/// The snippet body is a single `code` blob (lines joined by `\n`) beginning at
/// `snippet_start`, rather than an array of per-line `{line, text}` objects.
/// Line numbers are implicit (`snippet_start + i`), so the field names and line
/// numbers are not repeated on the wire — the dominant cost in a packed bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackItem {
    pub path: String,
    pub lang: String,
    pub name: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// First line number of `code` (usually equals `line_start`).
    pub snippet_start: u32,
    /// Snippet body: the symbol's lines joined by `\n`, possibly truncated.
    pub code: String,
    /// Why this item was included (e.g. "match", "central", "callee of X").
    pub reason: String,
    pub score: f32,
}

/// A budget-bounded bundle of context for a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPack {
    pub task: String,
    pub budget_tokens: u64,
    pub used_tokens: u64,
    /// True if relevant items were dropped to stay within budget.
    pub truncated: bool,
    pub items: Vec<PackItem>,
}

/// Conservative chars-per-token estimate (matches the savings accounting).
pub const CHARS_PER_TOKEN: u64 = 4;

/// Estimate the token cost of a string.
pub fn est_tokens(chars: u64) -> u64 {
    chars / CHARS_PER_TOKEN
}

/// Tokenize a free-form task into lowercased search terms: split on
/// non-identifier characters, then split camelCase/snake_case, dropping
/// stopwords and 1-character noise.
pub fn tokenize(task: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in task.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if raw.is_empty() {
            continue;
        }
        for tok in split_identifier(raw) {
            if tok.len() < 2 || is_stopword(&tok) {
                continue;
            }
            if seen.insert(tok.clone()) {
                terms.push(tok);
            }
        }
    }
    terms
}

fn is_stopword(t: &str) -> bool {
    matches!(
        t,
        "the"
            | "a"
            | "an"
            | "of"
            | "to"
            | "in"
            | "is"
            | "for"
            | "and"
            | "or"
            | "how"
            | "where"
            | "what"
            | "does"
            | "do"
            | "with"
            | "on"
            | "by"
            | "this"
            | "that"
            | "it"
            | "be"
            | "as"
            | "at"
            | "we"
            | "i"
            | "add"
            | "fix"
            | "use"
            | "using"
            | "make"
            | "get"
            | "set"
            | "all"
            | "when"
            | "from"
            | "into"
            | "via"
            | "can"
            | "should"
            | "code"
            | "function"
            | "method"
    )
}

/// Lexical relevance of a symbol to the task terms.
pub fn lexical_score(
    name: &str,
    kind: &str,
    signature: Option<&str>,
    container: Option<&str>,
    path: &str,
    terms: &[String],
) -> f32 {
    if terms.is_empty() {
        return 0.0;
    }
    let name_lower = name.to_ascii_lowercase();
    let name_tokens = split_identifier(name);
    let sig_lower = signature.map(|s| s.to_ascii_lowercase());
    let cont_tokens = container.map(split_identifier).unwrap_or_default();
    let path_lower = path.to_ascii_lowercase();

    let mut score = 0.0f32;
    for term in terms {
        if name_lower == *term {
            score += 20.0;
        } else if name_tokens.iter().any(|t| t == term) {
            score += 12.0;
        } else if name_lower.contains(term.as_str()) {
            score += 6.0;
        }
        if cont_tokens.iter().any(|t| t == term) {
            score += 4.0;
        }
        if let Some(sig) = &sig_lower {
            if sig.contains(term.as_str()) {
                score += 3.0;
            }
        }
        if path_lower.contains(term.as_str()) {
            score += 2.0;
        }
    }
    if score > 0.0 && is_priority_kind(kind) {
        score += 2.0;
    }
    score
}

fn is_priority_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function"
            | "method"
            | "struct"
            | "class"
            | "trait"
            | "interface"
            | "enum"
            | "type"
            | "constructor"
            | "module"
    )
}

/// Split an identifier into lowercase tokens on camelCase and snake/kebab.
pub fn split_identifier(s: &str) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_and_drops_stopwords() {
        let t = tokenize("How does the SegmentWriter flush to disk?");
        assert!(t.contains(&"segment".to_string()));
        assert!(t.contains(&"writer".to_string()));
        assert!(t.contains(&"flush".to_string()));
        assert!(t.contains(&"disk".to_string()));
        assert!(!t.contains(&"the".to_string()));
        assert!(!t.contains(&"how".to_string()));
    }

    #[test]
    fn scores_name_matches_highest() {
        let terms = tokenize("flush segment writer");
        let exact = lexical_score("flush", "function", None, None, "src/a.rs", &terms);
        let unrelated = lexical_score("zebra", "function", None, None, "src/a.rs", &terms);
        assert!(exact > unrelated);
        assert_eq!(unrelated, 0.0);
    }
}
