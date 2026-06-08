//! Identifier resolution: turn a source position into the identifier under the
//! cursor, classify how it is used (call, method/member access, type position,
//! import), and provide per-language hints that bias definition ranking.
//!
//! This is the framework behind typed go-to-definition. A generic resolver
//! works for every tree-sitter language by locating the identifier node and its
//! syntactic context; per-language [`LangResolver`] configs add finer rules
//! (e.g. how imports are spelled, whether a receiver disambiguates a method).
//! Full type inference is out of scope; resolution combines scope, imports, and
//! the global symbol table, and reports a confidence so callers can degrade
//! gracefully.

use std::cell::RefCell;
use std::collections::HashMap;

use tree_sitter::{Node, Parser, Point};

use crate::lang::Language;

thread_local! {
    static RESOLVE_PARSERS: RefCell<HashMap<Language, Parser>> = RefCell::new(HashMap::new());
}

/// An identifier found at a source position, with how it is being used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentRef {
    pub name: String,
    /// True if this is the callee of a call expression.
    pub is_call: bool,
    /// True if this is the property/method of a member access (`recv.name`).
    pub is_member: bool,
    /// True if this sits in a type position (annotation, generic, etc.).
    pub is_type: bool,
    /// True if this appears inside an import/use statement.
    pub is_import: bool,
}

/// Per-language resolution rules. Most behavior is shared; this captures the
/// few places grammars diverge.
pub trait LangResolver: Send + Sync {
    fn language(&self) -> Language;

    /// Kinds that introduce a member access whose property identifier should be
    /// treated as a method/field (so `a.b` classifies `b` as a member).
    fn member_kinds(&self) -> &'static [&'static str];

    /// Kinds that represent a call/invocation.
    fn call_kinds(&self) -> &'static [&'static str];

    /// Kinds that represent a type annotation / type reference position.
    fn type_kinds(&self) -> &'static [&'static str] {
        &["type_identifier", "type_annotation", "generic_type", "type"]
    }

    /// Kinds that represent an import/use statement.
    fn import_kinds(&self) -> &'static [&'static str];
}

macro_rules! lang_resolver {
    ($name:ident, $lang:expr, member = $member:expr, call = $call:expr, import = $import:expr) => {
        struct $name;
        impl LangResolver for $name {
            fn language(&self) -> Language {
                $lang
            }
            fn member_kinds(&self) -> &'static [&'static str] {
                $member
            }
            fn call_kinds(&self) -> &'static [&'static str] {
                $call
            }
            fn import_kinds(&self) -> &'static [&'static str] {
                $import
            }
        }
    };
}

lang_resolver!(
    RustResolver,
    Language::Rust,
    member = &["field_expression"],
    call = &["call_expression", "macro_invocation"],
    import = &["use_declaration"]
);
lang_resolver!(
    PythonResolver,
    Language::Python,
    member = &["attribute"],
    call = &["call"],
    import = &["import_statement", "import_from_statement"]
);
lang_resolver!(
    JsResolver,
    Language::JavaScript,
    member = &["member_expression"],
    call = &["call_expression", "new_expression"],
    import = &["import_statement"]
);
lang_resolver!(
    TsResolver,
    Language::TypeScript,
    member = &["member_expression"],
    call = &["call_expression", "new_expression"],
    import = &["import_statement"]
);
lang_resolver!(
    TsxResolver,
    Language::Tsx,
    member = &["member_expression"],
    call = &["call_expression", "new_expression"],
    import = &["import_statement"]
);
lang_resolver!(
    GoResolver,
    Language::Go,
    member = &["selector_expression"],
    call = &["call_expression"],
    import = &["import_spec"]
);
lang_resolver!(
    DartResolver,
    Language::Dart,
    member = &["member_expression"],
    call = &["call_expression", "constructor_invocation"],
    import = &["library_import", "import_specification"]
);

/// The per-language resolver for `lang`, or a generic fallback.
pub fn resolver_for(lang: Language) -> Box<dyn LangResolver> {
    match lang {
        Language::Rust => Box::new(RustResolver),
        Language::Python => Box::new(PythonResolver),
        Language::JavaScript => Box::new(JsResolver),
        Language::TypeScript => Box::new(TsResolver),
        Language::Tsx => Box::new(TsxResolver),
        Language::Go => Box::new(GoResolver),
        Language::Dart => Box::new(DartResolver),
        other => Box::new(GenericResolver(other)),
    }
}

/// A best-effort resolver for languages without a specialized config.
struct GenericResolver(Language);
impl LangResolver for GenericResolver {
    fn language(&self) -> Language {
        self.0
    }
    fn member_kinds(&self) -> &'static [&'static str] {
        &[
            "field_expression",
            "member_expression",
            "member_access_expression",
            "selector_expression",
            "attribute",
            "scoped_identifier",
        ]
    }
    fn call_kinds(&self) -> &'static [&'static str] {
        &[
            "call_expression",
            "call",
            "method_invocation",
            "invocation_expression",
            "function_call_expression",
            "member_call_expression",
            "object_creation_expression",
        ]
    }
    fn import_kinds(&self) -> &'static [&'static str] {
        &[
            "use_declaration",
            "import_statement",
            "import_from_statement",
            "import_declaration",
            "import_spec",
            "using_directive",
            "namespace_use_declaration",
        ]
    }
}

fn is_ident_kind(kind: &str) -> bool {
    kind.ends_with("identifier") || kind == "name" || kind == "constant" || kind == "property"
}

/// Locate the identifier under (1-based) `line`/`col` and classify its usage.
pub fn identifier_at(lang: Language, source: &[u8], line: u32, col: u32) -> Option<IdentRef> {
    let grammar = lang.grammar()?;
    let res = resolver_for(lang);
    RESOLVE_PARSERS.with(|cell| {
        let mut map = cell.borrow_mut();
        let parser = map.entry(lang).or_insert_with(|| {
            let mut p = Parser::new();
            let _ = p.set_language(&grammar);
            p
        });
        let tree = parser.parse(source, None)?;
        let point = Point {
            row: line.saturating_sub(1) as usize,
            column: col.saturating_sub(1) as usize,
        };
        let root = tree.root_node();
        // Smallest named node at the point; usually the identifier itself.
        let node = root.named_descendant_for_point_range(point, point)?;
        let node = if is_ident_kind(node.kind()) {
            node
        } else {
            // Otherwise look for an identifier-like child covering the point.
            ident_child_at(node, point)?
        };
        let name = node_text(node, source)?;

        let is_import = ancestor_in(node, res.import_kinds());
        let is_member = parent_in(node, res.member_kinds()) && !is_first_named_child(node);
        let is_type = ancestor_in(node, res.type_kinds());
        let is_call = is_callee(node, res.call_kinds());

        Some(IdentRef {
            name,
            is_call,
            is_member,
            is_type,
            is_import,
        })
    })
}

/// Find an identifier-like child of `node` whose byte range contains `point`.
fn ident_child_at<'t>(node: Node<'t>, point: Point) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let s = child.start_position();
        let e = child.end_position();
        let contains = (s.row, s.column) <= (point.row, point.column)
            && (point.row, point.column) <= (e.row, e.column);
        if contains {
            if is_ident_kind(child.kind()) {
                return Some(child);
            }
            if let Some(found) = ident_child_at(child, point) {
                return Some(found);
            }
        }
    }
    None
}

fn node_text(node: Node, source: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(source.get(node.start_byte()..node.end_byte())?).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn parent_in(node: Node, kinds: &[&str]) -> bool {
    node.parent()
        .map(|p| kinds.contains(&p.kind()))
        .unwrap_or(false)
}

fn ancestor_in(node: Node, kinds: &[&str]) -> bool {
    let mut cur = node.parent();
    let mut hops = 0;
    while let Some(p) = cur {
        if kinds.contains(&p.kind()) {
            return true;
        }
        hops += 1;
        if hops > 8 {
            break;
        }
        cur = p.parent();
    }
    false
}

fn is_first_named_child(node: Node) -> bool {
    if let Some(p) = node.parent() {
        let mut cursor = p.walk();
        let first = p.named_children(&mut cursor).next();
        if let Some(first) = first {
            return first.id() == node.id();
        }
    }
    false
}

/// True if `node` is the callee identifier of a call expression (directly, or
/// as the trailing member of a member-access callee).
fn is_callee(node: Node, call_kinds: &[&str]) -> bool {
    let parent = match node.parent() {
        Some(p) => p,
        None => return false,
    };
    if call_kinds.contains(&parent.kind()) {
        // Directly the function child of a call.
        if let Some(f) = parent.child_by_field_name("function") {
            return f.id() == node.id();
        }
        return is_first_named_child(node);
    }
    // `recv.method()` — node is the property of a member access that is the
    // callee of an enclosing call.
    if let Some(grand) = parent.parent() {
        if call_kinds.contains(&grand.kind()) {
            let callee = grand
                .child_by_field_name("function")
                .or_else(|| grand.named_child(0));
            if let Some(c) = callee {
                return c.id() == parent.id() && !is_first_named_child(node);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_call_identifier() {
        let src = b"fn main() {\n    helper();\n}\n";
        let r = identifier_at(Language::Rust, src, 2, 5).expect("ident");
        assert_eq!(r.name, "helper");
        assert!(r.is_call, "expected call: {r:?}");
        assert!(!r.is_member);
    }

    #[test]
    fn finds_member_method() {
        let src = b"fn main() {\n    obj.run();\n}\n";
        // Column of `run`.
        let r = identifier_at(Language::Rust, src, 2, 9).expect("ident");
        assert_eq!(r.name, "run");
        assert!(r.is_member, "expected member: {r:?}");
    }

    #[test]
    fn finds_import_identifier() {
        let src = b"from os import path\n";
        let r = identifier_at(Language::Python, src, 1, 16).expect("ident");
        assert_eq!(r.name, "path");
        assert!(r.is_import, "expected import: {r:?}");
    }
}
