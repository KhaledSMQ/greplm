//! Structural (AST) search.
//!
//! Two input dialects are accepted:
//!   * A native tree-sitter query S-expression (anything starting with `(`),
//!     with captures (`@name`) and `#eq?` / `#match?` predicates. This is the
//!     full power of the tree-sitter query language.
//!   * A friendlier code pattern with `$NAME` meta-variables, e.g.
//!     `fn $NAME($PARAMS) -> Result<$T>`, which is compiled into a tree-sitter
//!     query: concrete tokens become structure + `#eq?` constraints, and each
//!     meta-variable becomes a wildcard capture. A variadic `$$$` (optionally
//!     `$$$NAME`) matches any sequence of sibling nodes, so
//!     `function $NAME($$$) { $$$ }` matches a function with any parameters and
//!     any body. Variadic meta-variables only relax structure; they do not bind
//!     a capture.
//!
//! Matching runs the compiled query over candidate documents. Literal tokens in
//! the pattern double as trigram anchors so the index prunes candidates before
//! parsing.

use std::cell::RefCell;
use std::collections::HashMap;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

use crate::error::{Error, Result};
use crate::lang::Language;

thread_local! {
    static STRUCT_PARSERS: RefCell<HashMap<Language, Parser>> = RefCell::new(HashMap::new());
}

/// Sentinel prefix used when substituting `$NAME` meta-variables so the pattern
/// still parses as code before we walk it.
const SENTINEL: &str = "GREPLMMV";

/// The capture automatically attached to the root of a compiled pattern, used
/// to locate the matched node.
const ROOT_CAPTURE: &str = "greplm.match";

/// Sentinel that a variadic `$$$` meta-variable is collapsed to. It parses as
/// an identifier in the common contexts (parameter lists, argument lists,
/// statement blocks, arrays) and is dropped during emission, leaving the
/// surrounding structure unconstrained — tree-sitter queries already allow
/// extra, unmatched sibling nodes, which is exactly variadic semantics.
const VARIADIC: &str = "GREPLMVARIADIC";

/// A compiled structural pattern ready to run against documents.
pub struct Compiled {
    query: Query,
    /// Literal tokens that must appear in any matching document (from `#eq?`
    /// constraints); used as a trigram prefilter. Empty disables prefiltering.
    pub anchors: Vec<String>,
    root_capture: Option<u32>,
}

/// A capture bound by a structural match.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StructCapture {
    pub name: String,
    pub text: String,
    pub line: u32,
}

/// A single structural match within one document.
#[derive(Debug, Clone)]
pub struct StructMatch {
    pub line_start: u32,
    pub line_end: u32,
    pub kind: String,
    pub captures: Vec<StructCapture>,
}

/// Compile a pattern (S-expression or meta-variable form) for `lang`.
pub fn compile(lang: Language, pattern: &str) -> Result<Compiled> {
    let grammar = lang
        .grammar()
        .ok_or_else(|| Error::other(format!("language {} is not parseable", lang.id())))?;

    let trimmed = pattern.trim();
    let (query_src, anchors) = if trimmed.starts_with('(') {
        // Raw tree-sitter query: trust the user, no prefilter anchors.
        (trimmed.to_string(), Vec::new())
    } else {
        compile_metavars(lang, trimmed)?
    };

    let query = Query::new(&grammar, &query_src).map_err(|e| {
        Error::other(format!(
            "invalid structural query: {e}\n--- compiled query ---\n{query_src}"
        ))
    })?;
    let root_capture = query
        .capture_names()
        .iter()
        .position(|n| *n == ROOT_CAPTURE)
        .map(|i| i as u32);
    Ok(Compiled {
        query,
        anchors,
        root_capture,
    })
}

/// Run a compiled pattern over one source buffer.
pub fn run(lang: Language, compiled: &Compiled, source: &[u8]) -> Vec<StructMatch> {
    let grammar = match lang.grammar() {
        Some(g) => g,
        None => return Vec::new(),
    };
    STRUCT_PARSERS.with(|cell| {
        let mut map = cell.borrow_mut();
        let parser = map.entry(lang).or_insert_with(|| {
            let mut p = Parser::new();
            let _ = p.set_language(&grammar);
            p
        });
        let tree = match parser.parse(source, None) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let names = compiled.query.capture_names();
        // QueryCursor's match limit defaults to `u32::MAX` (effectively
        // unlimited), so matches are not silently capped on large files.
        let mut cursor = QueryCursor::new();
        let mut out = Vec::new();
        let mut b1 = Vec::new();
        let mut b2 = Vec::new();
        let mut src = source;
        let mut matches = cursor.matches(&compiled.query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            if !m.satisfies_text_predicates(&compiled.query, &mut b1, &mut b2, &mut src) {
                continue;
            }
            // Locate the primary node: the root capture if present, else the
            // first capture.
            let primary = compiled
                .root_capture
                .and_then(|ri| m.captures.iter().find(|c| c.index == ri))
                .or_else(|| m.captures.first());
            let node = match primary {
                Some(c) => c.node,
                None => continue,
            };
            let mut captures = Vec::new();
            for c in m.captures {
                let name = names.get(c.index as usize).copied().unwrap_or("");
                if name == ROOT_CAPTURE {
                    continue;
                }
                if let Ok(text) = std::str::from_utf8(&source[c.node.byte_range()]) {
                    captures.push(StructCapture {
                        name: name.to_string(),
                        text: text.to_string(),
                        line: c.node.start_position().row as u32 + 1,
                    });
                }
            }
            out.push(StructMatch {
                line_start: node.start_position().row as u32 + 1,
                line_end: node.end_position().row as u32 + 1,
                kind: node.kind().to_string(),
                captures,
            });
        }
        out
    })
}

/// Compile a `$NAME` meta-variable pattern into a tree-sitter query, returning
/// the query text and the literal trigram anchors.
fn compile_metavars(lang: Language, pattern: &str) -> Result<(String, Vec<String>)> {
    let grammar = lang
        .grammar()
        .ok_or_else(|| Error::other(format!("language {} is not parseable", lang.id())))?;

    // Substitute `$NAME` with a parseable sentinel identifier we can recognize.
    let (substituted, _names) = substitute_metavars(pattern);

    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|e| Error::other(format!("set_language: {e}")))?;
    let tree = parser
        .parse(substituted.as_bytes(), None)
        .ok_or_else(|| Error::other("failed to parse pattern".to_string()))?;
    let src = substituted.as_bytes();

    // Find the meaningful root by unwrapping single-child wrapper nodes.
    let root = unwrap_root(tree.root_node());
    if root.is_error() {
        return Err(Error::other(
            "pattern did not parse cleanly; check the syntax or use a tree-sitter query"
                .to_string(),
        ));
    }

    let mut out = String::new();
    let mut anchors = Vec::new();
    let mut counter = 0usize;
    emit(root, src, &mut out, &mut anchors, &mut counter);
    out.push_str(&format!(" @{ROOT_CAPTURE}"));
    Ok((out, anchors))
}

/// Replace `$Ident` occurrences with `GREPLMMV_Ident` sentinels.
fn substitute_metavars(pattern: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(pattern.len());
    let mut names = Vec::new();
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            // Variadic `$$$` (optionally `$$$NAME`): collapse to a single
            // sentinel identifier that we later drop from the query.
            if i + 2 < bytes.len() && bytes[i + 1] == b'$' && bytes[i + 2] == b'$' {
                let mut j = i + 3;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                out.push_str(VARIADIC);
                i = j;
                continue;
            }
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > i + 1 {
                let name = &pattern[i + 1..j];
                out.push_str(SENTINEL);
                out.push('_');
                out.push_str(name);
                names.push(name.to_string());
                i = j;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    (out, names)
}

/// Unwrap single-named-child wrapper nodes (file/program/statement shells) to
/// reach the meaningful pattern node.
fn unwrap_root(node: Node) -> Node {
    const WRAPPERS: &[&str] = &[
        "source_file",
        "program",
        "translation_unit",
        "module",
        "expression_statement",
        "statement",
        "compound_statement",
        "block",
    ];
    let mut cur = node;
    loop {
        if !WRAPPERS.contains(&cur.kind()) {
            return cur;
        }
        let mut cursor = cur.walk();
        let children: Vec<Node> = cur.named_children(&mut cursor).collect();
        if children.len() == 1 {
            cur = children[0];
        } else {
            return cur;
        }
    }
}

/// Recursively emit an S-expression for `node`.
fn emit(node: Node, src: &[u8], out: &mut String, anchors: &mut Vec<String>, counter: &mut usize) {
    let text = node_text(node, src);

    // A variadic `$$$` node (and any single-child wrapper around it, such as an
    // expression statement) contributes no constraint: emit nothing so the
    // parent matches regardless of the nodes in this position.
    if let Some(t) = &text {
        if t.trim() == VARIADIC {
            return;
        }
    }

    // A substituted meta-variable becomes a wildcard capture.
    if let Some(t) = &text {
        if let Some(name) = t.strip_prefix(&format!("{SENTINEL}_")) {
            if is_clean_ident(name) {
                out.push_str(&format!("(_) @{name}"));
                return;
            }
        }
    }

    let mut cursor = node.walk();
    let named: Vec<Node> = node.named_children(&mut cursor).collect();

    if node.child_count() == 0 {
        // A true terminal token (identifier, literal, keyword). Constrain by
        // kind and exact text.
        out.push_str(&format!("({})", node.kind()));
        if let Some(t) = text {
            if !t.is_empty() && !t.starts_with(SENTINEL) {
                *counter += 1;
                let cap = format!("greplm_a{counter}");
                out.push_str(&format!(" @{cap}"));
                out.push_str(&format!(" (#eq? @{cap} \"{}\")", escape(&t)));
                if t.len() >= 3 && t.chars().all(|c| !c.is_whitespace()) {
                    anchors.push(t);
                }
            }
        }
        return;
    }

    if named.is_empty() {
        // Has only anonymous children (e.g. empty `()` or `{}`): match any node
        // of this kind without constraining its contents.
        out.push_str(&format!("({})", node.kind()));
        return;
    }

    out.push('(');
    out.push_str(node.kind());
    for child in named {
        out.push(' ');
        emit(child, src, out, anchors, counter);
    }
    out.push(')');
}

fn node_text(node: Node, src: &[u8]) -> Option<String> {
    std::str::from_utf8(src.get(node.start_byte()..node.end_byte())?)
        .ok()
        .map(|s| s.to_string())
}

fn is_clean_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_query_matches_calls() {
        let src = b"fn main() {\n    foo();\n    bar();\n}\n";
        let c = compile(
            Language::Rust,
            "(call_expression function: (identifier) @fn)",
        )
        .unwrap();
        let m = run(Language::Rust, &c, src);
        let names: Vec<&str> = m
            .iter()
            .flat_map(|mm| mm.captures.iter())
            .map(|c| c.text.as_str())
            .collect();
        assert!(names.contains(&"foo"), "got {names:?}");
        assert!(names.contains(&"bar"), "got {names:?}");
    }

    #[test]
    fn predicate_filters_by_name() {
        let src = b"fn main() {\n    foo();\n    bar();\n}\n";
        let c = compile(
            Language::Rust,
            "((call_expression function: (identifier) @fn) (#eq? @fn \"foo\"))",
        )
        .unwrap();
        let m = run(Language::Rust, &c, src);
        let names: Vec<&str> = m
            .iter()
            .flat_map(|mm| mm.captures.iter())
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(names, vec!["foo"], "predicate should keep only foo");
    }

    #[test]
    fn metavar_pattern_compiles_and_matches() {
        let src = b"struct A;\nfn alpha() {}\nfn beta() {}\n";
        let c = compile(Language::Rust, "fn $NAME() {}").unwrap();
        let m = run(Language::Rust, &c, src);
        // Both functions should match the shape; capture NAME bound.
        let names: Vec<String> = m
            .iter()
            .flat_map(|mm| mm.captures.iter())
            .filter(|c| c.name == "NAME")
            .map(|c| c.text.clone())
            .collect();
        assert!(names.contains(&"alpha".to_string()), "got {names:?}");
        assert!(names.contains(&"beta".to_string()), "got {names:?}");
    }

    #[test]
    fn variadic_metavars_match_any_params_and_body() {
        let src = b"function noop() {}\nfunction add(a, b) { return a + b; }\n";
        let c = compile(Language::JavaScript, "function $NAME($$$) { $$$ }").unwrap();
        let m = run(Language::JavaScript, &c, src);
        let names: Vec<String> = m
            .iter()
            .flat_map(|mm| mm.captures.iter())
            .filter(|c| c.name == "NAME")
            .map(|c| c.text.clone())
            .collect();
        assert!(names.contains(&"noop".to_string()), "got {names:?}");
        assert!(
            names.contains(&"add".to_string()),
            "variadic params/body should match a function with args, got {names:?}"
        );
    }
}
