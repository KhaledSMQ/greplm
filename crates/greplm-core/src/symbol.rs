//! Symbol extraction via tree-sitter.
//!
//! Rather than maintain a query file per language, we walk the parse tree and
//! match node kinds against a small per-language table of "definition" kinds.
//! The symbol name is taken from the `name` field when present, otherwise from
//! the `declarator` field (C/C++), otherwise the first identifier-like child.

use std::cell::RefCell;
use std::collections::HashMap;

use tree_sitter::{Node, Parser};

use crate::lang::Language;
use crate::segment::{RawRef, RawSymbol, RefKind};

thread_local! {
    /// One parser per language per worker thread, reused across files.
    static PARSERS: RefCell<HashMap<Language, Parser>> = RefCell::new(HashMap::new());
}

/// Parse `source` and extract top-level and nested symbol definitions.
pub fn extract(lang: Language, source: &[u8]) -> Vec<RawSymbol> {
    extract_all(lang, source).0
}

/// Parse `source` once and extract both symbol definitions and references
/// (call sites + imports). Parsing dominates the cost, so doing both walks
/// over a single parse tree keeps indexing cheap.
pub fn extract_all(lang: Language, source: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) {
    let grammar = match lang.grammar() {
        Some(g) => g,
        None => return (Vec::new(), Vec::new()),
    };
    PARSERS.with(|cell| {
        let mut map = cell.borrow_mut();
        let parser = map.entry(lang).or_insert_with(|| {
            let mut p = Parser::new();
            let _ = p.set_language(&grammar);
            p
        });
        let tree = match parser.parse(source, None) {
            Some(t) => t,
            None => return (Vec::new(), Vec::new()),
        };
        let syms = extract_tree(lang, &tree, source);
        let refs = extract_refs_tree(lang, &tree, source);
        (syms, refs)
    })
}

/// Walk a parsed tree, tracking the enclosing named container for each symbol.
fn extract_tree(lang: Language, tree: &tree_sitter::Tree, source: &[u8]) -> Vec<RawSymbol> {
    let mut out = Vec::new();
    let mut cursor = tree.root_node().walk();
    // Stack of (node, enclosing container name).
    let mut stack: Vec<(Node, Option<String>)> = vec![(tree.root_node(), None)];
    while let Some((node, container)) = stack.pop() {
        let mut child_container = container.clone();
        // A declaration whose node kind is itself a definition, or — for the
        // JS family — a function/arrow/class bound to a name via assignment or
        // a variable declarator (`const f = () => {}`, `exports.foo = fn`).
        let found = match symbol_kind(lang, node.kind()) {
            Some(kind) => node_name(node, source).map(|name| (name, kind)),
            None if matches!(
                lang,
                Language::JavaScript | Language::TypeScript | Language::Tsx
            ) =>
            {
                js_bound_symbol(node, source)
            }
            None => None,
        };
        if let Some((name, kind)) = found {
            out.push(RawSymbol {
                name: name.clone(),
                kind: kind.to_string(),
                line_start: node.start_position().row as u32 + 1,
                line_end: node.end_position().row as u32 + 1,
                container: container.clone(),
                signature: signature(node, source),
            });
            // Container-like symbols become the parent for their children.
            if is_container_kind(kind) {
                child_container = Some(name);
            }
        }
        for child in node.children(&mut cursor) {
            stack.push((child, child_container.clone()));
        }
    }
    out
}

/// Walk a parsed tree and collect references: function/method call sites and
/// imported names. Unlike whole-word text search, these are structural — a
/// `call` ref means the identifier was genuinely invoked, which is what the
/// call graph and blast-radius queries are built on.
fn extract_refs_tree(lang: Language, tree: &tree_sitter::Tree, source: &[u8]) -> Vec<RawRef> {
    let mut out = Vec::new();
    let mut cursor = tree.root_node().walk();
    let mut stack: Vec<Node> = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if is_call_kind(lang, kind) {
            if let Some(callee) = call_callee(node) {
                if let Some(id) = last_identifier(callee, source) {
                    out.push(RawRef {
                        name: id,
                        kind: RefKind::Call,
                        line: node.start_position().row as u32 + 1,
                        column: node.start_position().column as u32 + 1,
                    });
                }
            }
        } else if is_import_kind(lang, kind) {
            collect_import_names(node, source, &mut out);
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

/// Node kinds that represent a function/method/constructor invocation.
fn is_call_kind(lang: Language, kind: &str) -> bool {
    match lang {
        Language::Rust => matches!(kind, "call_expression" | "macro_invocation"),
        Language::Python => kind == "call",
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            matches!(kind, "call_expression" | "new_expression")
        }
        Language::Go => kind == "call_expression",
        Language::Java => matches!(kind, "method_invocation" | "object_creation_expression"),
        Language::C => kind == "call_expression",
        Language::Cpp => kind == "call_expression",
        Language::CSharp => {
            matches!(kind, "invocation_expression" | "object_creation_expression")
        }
        Language::Ruby => matches!(kind, "call" | "method_call" | "command"),
        Language::Php => matches!(
            kind,
            "function_call_expression"
                | "member_call_expression"
                | "scoped_call_expression"
                | "object_creation_expression"
        ),
        Language::Swift => kind == "call_expression",
        Language::Dart => {
            matches!(kind, "call_expression" | "constructor_invocation")
        }
        Language::Other => false,
    }
}

/// Node kinds that introduce imported/used names.
fn is_import_kind(lang: Language, kind: &str) -> bool {
    match lang {
        Language::Rust => kind == "use_declaration",
        Language::Python => matches!(kind, "import_statement" | "import_from_statement"),
        Language::JavaScript | Language::TypeScript | Language::Tsx => kind == "import_statement",
        Language::Go => kind == "import_spec",
        Language::Java => kind == "import_declaration",
        Language::CSharp => kind == "using_directive",
        Language::Php => kind == "namespace_use_declaration",
        Language::Swift => kind == "import_declaration",
        // Dart imports name a URI/package, not a symbol (like C/C++ includes),
        // so they are not useful name references.
        // C/C++ includes are file paths, not symbol names; Ruby require is a
        // plain call already captured as a "call" ref.
        _ => false,
    }
}

/// The node holding the called function/method, by trying the common field
/// names across grammars and falling back to the first named child.
fn call_callee(node: Node) -> Option<Node> {
    for field in ["function", "name", "method", "constructor"] {
        if let Some(n) = node.child_by_field_name(field) {
            return Some(n);
        }
    }
    let mut cursor = node.walk();
    let first = node.named_children(&mut cursor).next();
    first
}

/// Descend to the right-most identifier-like leaf, so `a.b.c()` resolves to
/// `c`, `mod::path::f()` to `f`, and a bare `foo()` to `foo`.
fn last_identifier(node: Node, source: &[u8]) -> Option<String> {
    let k = node.kind();
    if (k.ends_with("identifier") || k == "constant" || k == "name") && node.child_count() == 0 {
        return text(node, source);
    }
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    for child in children.into_iter().rev() {
        if let Some(found) = last_identifier(child, source) {
            return Some(found);
        }
    }
    // Leaf identifier with a non-zero child count is unusual; fall back to text.
    if k.ends_with("identifier") || k == "constant" || k == "name" {
        return text(node, source);
    }
    None
}

/// Collect identifier-like leaves under an import statement (bounded depth) as
/// `import` refs. Best-effort across grammars; aids definition resolution.
fn collect_import_names(node: Node, source: &[u8], out: &mut Vec<RawRef>) {
    fn walk(node: Node, source: &[u8], depth: u32, line: u32, out: &mut Vec<RawRef>) {
        if depth == 0 {
            return;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let k = child.kind();
            if (k.ends_with("identifier") || k == "constant") && child.child_count() == 0 {
                if let Some(name) = text(child, source) {
                    out.push(RawRef {
                        name,
                        kind: RefKind::Import,
                        line,
                        column: child.start_position().column as u32 + 1,
                    });
                }
            } else {
                walk(child, source, depth - 1, line, out);
            }
        }
    }
    let line = node.start_position().row as u32 + 1;
    walk(node, source, 6, line, out);
}

fn is_container_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class"
            | "struct"
            | "trait"
            | "interface"
            | "enum"
            | "module"
            | "namespace"
            | "protocol"
            | "record"
            | "union"
    )
}

/// Extract a compact one-line signature from a definition node (its first line,
/// trimmed and bounded).
fn signature(node: Node, source: &[u8]) -> Option<String> {
    let bytes = source.get(node.start_byte()..node.end_byte())?;
    let text = std::str::from_utf8(bytes).ok()?;
    let first = text.lines().next()?.trim();
    let first = first.trim_end_matches('{').trim();
    if first.is_empty() {
        return None;
    }
    const MAX: usize = 200;
    if first.len() > MAX {
        let mut end = MAX;
        while !first.is_char_boundary(end) {
            end -= 1;
        }
        Some(first[..end].to_string())
    } else {
        Some(first.to_string())
    }
}

/// Map a tree-sitter node kind to a greplm symbol kind label, per language.
fn symbol_kind(lang: Language, node_kind: &str) -> Option<&'static str> {
    match lang {
        Language::Rust => match node_kind {
            "function_item" => Some("function"),
            "struct_item" => Some("struct"),
            "enum_item" => Some("enum"),
            "union_item" => Some("union"),
            "trait_item" => Some("trait"),
            "mod_item" => Some("module"),
            "macro_definition" => Some("macro"),
            "type_item" => Some("type"),
            "const_item" => Some("const"),
            "static_item" => Some("static"),
            _ => None,
        },
        Language::Python => match node_kind {
            "function_definition" => Some("function"),
            "class_definition" => Some("class"),
            _ => None,
        },
        Language::JavaScript => match node_kind {
            "function_declaration" | "generator_function_declaration" => Some("function"),
            "class_declaration" => Some("class"),
            "method_definition" => Some("method"),
            _ => None,
        },
        Language::TypeScript | Language::Tsx => match node_kind {
            "function_declaration" | "generator_function_declaration" => Some("function"),
            "class_declaration" | "abstract_class_declaration" => Some("class"),
            "method_definition" => Some("method"),
            "interface_declaration" => Some("interface"),
            "type_alias_declaration" => Some("type"),
            "enum_declaration" => Some("enum"),
            _ => None,
        },
        Language::Go => match node_kind {
            "function_declaration" => Some("function"),
            "method_declaration" => Some("method"),
            "type_spec" => Some("type"),
            _ => None,
        },
        Language::Java => match node_kind {
            "class_declaration" => Some("class"),
            "interface_declaration" => Some("interface"),
            "enum_declaration" => Some("enum"),
            "record_declaration" => Some("record"),
            "method_declaration" => Some("method"),
            "constructor_declaration" => Some("constructor"),
            _ => None,
        },
        Language::C => match node_kind {
            "function_definition" => Some("function"),
            "struct_specifier" => Some("struct"),
            "enum_specifier" => Some("enum"),
            "union_specifier" => Some("union"),
            "type_definition" => Some("type"),
            _ => None,
        },
        Language::Cpp => match node_kind {
            "function_definition" => Some("function"),
            "struct_specifier" => Some("struct"),
            "class_specifier" => Some("class"),
            "enum_specifier" => Some("enum"),
            "union_specifier" => Some("union"),
            "namespace_definition" => Some("namespace"),
            "type_definition" => Some("type"),
            _ => None,
        },
        Language::CSharp => match node_kind {
            "class_declaration" => Some("class"),
            "interface_declaration" => Some("interface"),
            "struct_declaration" => Some("struct"),
            "enum_declaration" => Some("enum"),
            "record_declaration" => Some("record"),
            "method_declaration" => Some("method"),
            "constructor_declaration" => Some("constructor"),
            "namespace_declaration" => Some("namespace"),
            _ => None,
        },
        Language::Ruby => match node_kind {
            "method" | "singleton_method" => Some("method"),
            "class" => Some("class"),
            "module" => Some("module"),
            _ => None,
        },
        Language::Php => match node_kind {
            "function_definition" => Some("function"),
            "method_declaration" => Some("method"),
            "class_declaration" => Some("class"),
            "interface_declaration" => Some("interface"),
            "trait_declaration" => Some("trait"),
            "enum_declaration" => Some("enum"),
            _ => None,
        },
        Language::Swift => match node_kind {
            "function_declaration" => Some("function"),
            "class_declaration" => Some("class"),
            "protocol_declaration" => Some("protocol"),
            _ => None,
        },
        // Dart names live in nested `*_signature` nodes, but we target the
        // `*_declaration` wrappers so a symbol's line range spans its body
        // (the call-graph relies on that to find calls inside it).
        Language::Dart => match node_kind {
            "function_declaration" => Some("function"),
            "method_declaration" => Some("method"),
            "getter_declaration" | "external_getter_declaration" => Some("getter"),
            "setter_declaration" | "external_setter_declaration" => Some("setter"),
            "constructor_signature" | "factory_constructor_signature" => Some("constructor"),
            "class_declaration" => Some("class"),
            "mixin_declaration" => Some("mixin"),
            "enum_declaration" => Some("enum"),
            "extension_declaration" | "extension_type_declaration" => Some("extension"),
            "type_alias" => Some("type"),
            _ => None,
        },
        Language::Other => None,
    }
}

/// JS/TS: a function, arrow, generator, or class *expression* bound to a name,
/// which the plain node-kind table misses because the definition is anonymous.
/// Handles `const f = () => {}` / `let f = function () {}` (variable
/// declarators) and `x.y = fn` / `exports.foo = fn` (assignments). The
/// anonymous default export `module.exports = fn` is skipped: its real identity
/// is the file, which is already indexed.
fn js_bound_symbol(node: Node, source: &[u8]) -> Option<(String, &'static str)> {
    match node.kind() {
        "variable_declarator" => {
            let value = node.child_by_field_name("value")?;
            let kind = js_value_kind(value)?;
            let name = node.child_by_field_name("name")?;
            if !name.kind().ends_with("identifier") {
                return None; // skip destructuring patterns
            }
            Some((text(name, source)?, kind))
        }
        "assignment_expression" => {
            let value = node.child_by_field_name("right")?;
            let kind = js_value_kind(value)?;
            let left = node.child_by_field_name("left")?;
            Some((js_assign_name(left, source)?, kind))
        }
        _ => None,
    }
}

/// The greplm symbol kind for a JS value node, if it is a callable/class
/// expression worth recording as a definition.
fn js_value_kind(value: Node) -> Option<&'static str> {
    match value.kind() {
        "function"
        | "function_expression"
        | "arrow_function"
        | "generator_function"
        | "generator_function_expression" => Some("function"),
        "class" => Some("class"),
        _ => None,
    }
}

/// The name to record for an assignment target: the trailing property of a
/// member expression (`a.b.c = fn` -> `c`, `exports.foo = fn` -> `foo`) or a
/// bare identifier. Returns `None` for the anonymous default export
/// (`module.exports = fn`, `exports = fn`).
fn js_assign_name(left: Node, source: &[u8]) -> Option<String> {
    match left.kind() {
        "identifier" => {
            let name = text(left, source)?;
            if name == "exports" {
                None
            } else {
                Some(name)
            }
        }
        "member_expression" => {
            let prop = left.child_by_field_name("property")?;
            let name = text(prop, source)?;
            if name == "exports" {
                None
            } else {
                Some(name)
            }
        }
        _ => None,
    }
}

fn node_name(node: Node, source: &[u8]) -> Option<String> {
    if let Some(n) = node.child_by_field_name("name") {
        return text(n, source);
    }
    if let Some(d) = node.child_by_field_name("declarator") {
        if let Some(id) = find_identifier(d, 4) {
            return text(id, source);
        }
    }
    find_identifier(node, 2).and_then(|n| text(n, source))
}

/// Find the first identifier-like node within `depth` levels (pre-order).
fn find_identifier(node: Node, depth: u32) -> Option<Node> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k.ends_with("identifier") || k == "constant" || k == "name" {
            return Some(child);
        }
    }
    if depth == 0 {
        return None;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_identifier(child, depth - 1) {
            return Some(found);
        }
    }
    None
}

fn text(node: Node, source: &[u8]) -> Option<String> {
    let bytes = source.get(node.start_byte()..node.end_byte())?;
    let s = std::str::from_utf8(bytes).ok()?.trim();
    if s.is_empty() || s.len() > 256 {
        None
    } else {
        Some(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_symbols() {
        let src = b"struct Foo;\nfn bar() {}\n";
        let syms = extract(Language::Rust, src);
        assert!(syms.iter().any(|s| s.name == "Foo" && s.kind == "struct"));
        assert!(syms.iter().any(|s| s.name == "bar" && s.kind == "function"));
    }

    #[test]
    fn extracts_rust_calls() {
        let src =
            b"fn caller() {\n    helper();\n    obj.method();\n    std::mem::take(&mut x);\n}\n";
        let refs = extract_all(Language::Rust, src).1;
        let calls: Vec<&str> = refs
            .iter()
            .filter(|r| r.kind == RefKind::Call)
            .map(|r| r.name.as_str())
            .collect();
        assert!(
            calls.contains(&"helper"),
            "expected helper call, got {refs:?}"
        );
        assert!(
            calls.contains(&"method"),
            "expected method call, got {refs:?}"
        );
        assert!(calls.contains(&"take"), "expected take call, got {refs:?}");
    }

    #[test]
    fn extracts_python_calls_and_imports() {
        let src = b"from os import path\nimport sys\n\ndef run():\n    path.join('a', 'b')\n    helper()\n";
        let refs = extract_all(Language::Python, src).1;
        let calls: Vec<&str> = refs
            .iter()
            .filter(|r| r.kind == RefKind::Call)
            .map(|r| r.name.as_str())
            .collect();
        let imports: Vec<&str> = refs
            .iter()
            .filter(|r| r.kind == RefKind::Import)
            .map(|r| r.name.as_str())
            .collect();
        assert!(calls.contains(&"join"), "expected join call, got {refs:?}");
        assert!(
            calls.contains(&"helper"),
            "expected helper call, got {refs:?}"
        );
        assert!(
            imports.contains(&"path"),
            "expected path import, got {refs:?}"
        );
    }

    #[test]
    fn extracts_js_name_bound_functions() {
        let src = b"const handler = (req, res) => { send(); };\n\
                    let make = function () { return 1; };\n\
                    exports.run = function (a) { return a; };\n\
                    module.exports = function (router) { return router; };\n";
        let syms = extract(Language::JavaScript, src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"handler"), "arrow binding, got {names:?}");
        assert!(
            names.contains(&"make"),
            "function expr binding, got {names:?}"
        );
        assert!(names.contains(&"run"), "exports.run binding, got {names:?}");
        // The anonymous default export must not be recorded as `exports`.
        assert!(
            !names.contains(&"exports"),
            "anonymous module.exports should be skipped, got {names:?}"
        );
        // The arrow's range should span its body so the call graph attributes
        // calls inside it.
        let handler = syms.iter().find(|s| s.name == "handler").unwrap();
        assert!(handler.line_start <= 1 && handler.line_end >= 1);
    }

    #[test]
    fn extracts_swift_symbols() {
        // Guards against tree-sitter-swift ABI drift: the grammar must parse and
        // yield symbols, not just construct a `tree_sitter::Language`.
        let src = b"class Greeter {\n    func greet() -> String { return \"hi\" }\n}\n";
        let syms = extract(Language::Swift, src);
        assert!(
            syms.iter()
                .any(|s| s.name == "Greeter" && s.kind == "class"),
            "expected Swift class, got {syms:?}"
        );
        assert!(
            syms.iter()
                .any(|s| s.name == "greet" && s.kind == "function"),
            "expected Swift function, got {syms:?}"
        );
    }

    #[test]
    fn extracts_dart_symbols_and_calls() {
        // Guards against tree-sitter-dart ABI drift and verifies that symbol
        // ranges span bodies so the call graph resolves calls within them.
        let src = b"class Counter {\n  int value = 0;\n  void increment() {\n    helper();\n  }\n}\n\nvoid helper() {\n  print('hi');\n}\n";
        let (syms, refs) = extract_all(Language::Dart, src);
        assert!(
            syms.iter()
                .any(|s| s.name == "Counter" && s.kind == "class"),
            "expected Dart class, got {syms:?}"
        );
        let inc = syms
            .iter()
            .find(|s| s.name == "increment" && s.kind == "method")
            .expect("expected Dart method increment");
        // The method's range must span its body (so the `helper()` call inside
        // it is attributable to `increment`).
        assert!(
            inc.line_start <= 4 && inc.line_end >= 4,
            "increment range should span its body, got {inc:?}"
        );
        assert!(
            syms.iter()
                .any(|s| s.name == "helper" && s.kind == "function"),
            "expected Dart top-level function, got {syms:?}"
        );
        let calls: Vec<&str> = refs
            .iter()
            .filter(|r| r.kind == RefKind::Call)
            .map(|r| r.name.as_str())
            .collect();
        assert!(
            calls.contains(&"helper"),
            "expected helper call, got {refs:?}"
        );
        assert!(
            calls.contains(&"print"),
            "expected print call, got {refs:?}"
        );
    }
}
