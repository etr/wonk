//! File indexing and tree-sitter parsing.
//!
//! Provides multi-language parsing infrastructure: language detection by file
//! extension, parser construction with the correct grammar, and file parsing.
//! Also extracts symbol definitions (functions, classes, types, etc.) from
//! parsed syntax trees across all supported languages.

use std::path::Path;

use tree_sitter::{Language, Node, Parser, Tree};

use crate::types::{FileImports, RawTypeEdge, Reference, ReferenceKind, Symbol, SymbolKind};

/// Supported programming languages with bundled Tree-sitter grammars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Rust,
    Go,
    Java,
    C,
    Cpp,
    Ruby,
    Php,
    CSharp,
}

impl Lang {
    /// Returns the human-readable name for this language.
    pub fn name(self) -> &'static str {
        match self {
            Lang::TypeScript => "TypeScript",
            Lang::Tsx => "TSX",
            Lang::JavaScript => "JavaScript",
            Lang::Python => "Python",
            Lang::Rust => "Rust",
            Lang::Go => "Go",
            Lang::Java => "Java",
            Lang::C => "C",
            Lang::Cpp => "C++",
            Lang::Ruby => "Ruby",
            Lang::Php => "PHP",
            Lang::CSharp => "C#",
        }
    }
}

/// Detect the programming language of a file based on its extension.
///
/// Returns `None` for unsupported or missing extensions.
pub fn detect_language(path: &Path) -> Option<Lang> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "ts" => Some(Lang::TypeScript),
        "tsx" => Some(Lang::Tsx),
        "js" | "jsx" => Some(Lang::JavaScript),
        "py" => Some(Lang::Python),
        "rs" => Some(Lang::Rust),
        "go" => Some(Lang::Go),
        "java" => Some(Lang::Java),
        "c" | "h" => Some(Lang::C),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(Lang::Cpp),
        "rb" => Some(Lang::Ruby),
        "php" => Some(Lang::Php),
        "cs" => Some(Lang::CSharp),
        _ => None,
    }
}

/// Return the Tree-sitter [`Language`] grammar for the given language.
fn grammar_for(lang: Lang) -> Language {
    match lang {
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
        Lang::Java => tree_sitter_java::LANGUAGE.into(),
        Lang::C => tree_sitter_c::LANGUAGE.into(),
        Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        Lang::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
    }
}

/// Create a new [`Parser`] configured for the given language.
pub fn get_parser(lang: Lang) -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&grammar_for(lang))
        .expect("Error loading grammar — ABI version mismatch");
    parser
}

/// Parse a source file, returning the syntax tree and detected language.
///
/// Returns `None` when:
/// - the file extension is unsupported
/// - the file cannot be read
/// - the parser fails to produce a tree
pub fn parse_file(path: &Path) -> Option<(Tree, Lang)> {
    let lang = detect_language(path)?;
    let source = std::fs::read(path).ok()?;
    let mut parser = get_parser(lang);
    let tree = parser.parse(&source, None)?;
    Some((tree, lang))
}

// ---------------------------------------------------------------------------
// Symbol extraction
// ---------------------------------------------------------------------------

/// Extract symbol definitions from a parsed syntax tree.
///
/// Walks the tree with a cursor and matches node kinds specific to each
/// language to find function, class, struct, enum, trait, type alias,
/// constant and variable definitions.
pub fn extract_symbols(tree: &Tree, source: &str, file: &str, lang: Lang) -> Vec<Symbol> {
    let src = source.as_bytes();
    let mut symbols = Vec::new();
    let root = tree.root_node();

    walk_node(root, src, file, lang, None, &mut symbols);
    symbols
}

/// Recursively walk a node and its children, collecting symbols.
fn walk_node(
    node: Node,
    src: &[u8],
    file: &str,
    lang: Lang,
    scope: Option<&str>,
    symbols: &mut Vec<Symbol>,
) {
    let kind = node.kind();

    // Attempt to extract a symbol from this node.
    if let Some(sym) = match_node(node, kind, src, file, lang, scope) {
        let new_scope = sym.name.clone();
        symbols.push(sym);

        // For container nodes (class, struct, impl, trait, module, etc.),
        // recurse with updated scope.
        if is_container(kind, lang) {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32) {
                    walk_node(child, src, file, lang, Some(&new_scope), symbols);
                }
            }
            return; // already recursed children
        }
    }

    // Default: recurse into children with same scope.
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            walk_node(child, src, file, lang, scope, symbols);
        }
    }
}

/// Returns true if a node kind represents a container whose children should
/// be scoped under this symbol's name.
fn is_container(kind: &str, lang: Lang) -> bool {
    match lang {
        Lang::Rust => matches!(
            kind,
            "impl_item" | "trait_item" | "mod_item" | "struct_item" | "enum_item"
        ),
        Lang::Python => matches!(kind, "class_definition"),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            matches!(
                kind,
                "class_declaration" | "class" | "interface_declaration"
            )
        }
        Lang::Java => matches!(
            kind,
            "class_declaration" | "interface_declaration" | "enum_declaration"
        ),
        Lang::Go => false, // Go has no nested containers
        Lang::C | Lang::Cpp => matches!(
            kind,
            "class_specifier" | "struct_specifier" | "namespace_definition"
        ),
        Lang::Ruby => matches!(kind, "class" | "module"),
        Lang::Php => matches!(
            kind,
            "class_declaration" | "interface_declaration" | "trait_declaration"
        ),
        Lang::CSharp => matches!(
            kind,
            "class_declaration"
                | "struct_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "namespace_declaration"
                | "record_declaration"
        ),
    }
}

/// Try to extract a `Symbol` from a tree-sitter node.
fn match_node(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    lang: Lang,
    scope: Option<&str>,
) -> Option<Symbol> {
    match lang {
        Lang::Rust => extract_rust(node, kind, src, file, scope),
        Lang::Python => extract_python(node, kind, src, file, scope),
        Lang::JavaScript => extract_javascript(node, kind, src, file, scope),
        Lang::TypeScript | Lang::Tsx => extract_typescript(node, kind, src, file, lang, scope),
        Lang::Go => extract_go(node, kind, src, file, scope),
        Lang::Java => extract_java(node, kind, src, file, scope),
        Lang::C => extract_c(node, kind, src, file, scope),
        Lang::Cpp => extract_cpp(node, kind, src, file, scope),
        Lang::Ruby => extract_ruby(node, kind, src, file, scope),
        Lang::Php => extract_php(node, kind, src, file, scope),
        Lang::CSharp => extract_csharp(node, kind, src, file, scope),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get the text content of a node.
fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// Find a named child by its field name and return its text.
fn field_text<'a>(node: Node, field: &str, src: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name(field).map(|n| node_text(n, src))
}

/// Extract the first line of a node's text as the signature.
fn first_line(node: Node, src: &[u8]) -> String {
    let text = node_text(node, src);
    // Take everything up to the first '{' or newline, trimmed.
    let sig = text
        .split_once('{')
        .map(|(before, _)| before.trim())
        .unwrap_or_else(|| text.lines().next().unwrap_or("").trim());
    sig.to_string()
}

/// Build a `Symbol` with common fields pre-filled.
fn make_symbol(
    name: &str,
    kind: SymbolKind,
    node: Node,
    src: &[u8],
    file: &str,
    lang: Lang,
    scope: Option<&str>,
) -> Symbol {
    Symbol {
        name: name.to_string(),
        kind,
        file: file.to_string(),
        line: node.start_position().row + 1,
        col: node.start_position().column,
        end_line: Some(node.end_position().row + 1),
        scope: scope.map(|s| s.to_string()),
        signature: first_line(node, src),
        language: lang.name().to_string(),
        doc_comment: extract_doc_comment(node, src, lang),
    }
}

/// Maximum length for extracted doc comments.
const MAX_DOC_COMMENT_LEN: usize = 200;

/// Extract a doc comment for the given symbol node.
fn extract_doc_comment(node: Node, src: &[u8], lang: Lang) -> Option<String> {
    match lang {
        Lang::Python => extract_python_docstring(node, src),
        _ => extract_preceding_comment(node, src, lang),
    }
}

/// Extract a Python docstring: first `expression_statement > string` in the `body` field.
fn extract_python_docstring(node: Node, src: &[u8]) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    let first_stmt = body.named_child(0)?;
    if first_stmt.kind() != "expression_statement" {
        return None;
    }
    let string_node = first_stmt.named_child(0)?;
    if string_node.kind() != "string" {
        return None;
    }
    let text = node_text(string_node, src);
    // Strip quotes: """...""", '''...''', "...", '...'
    let stripped = text
        .strip_prefix("\"\"\"")
        .and_then(|s| s.strip_suffix("\"\"\""))
        .or_else(|| text.strip_prefix("'''").and_then(|s| s.strip_suffix("'''")))
        .or_else(|| text.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .or_else(|| text.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(text);
    let cleaned = strip_doc_prefix(stripped, Lang::Python);
    if cleaned.is_empty() {
        return None;
    }
    Some(truncate_doc(&cleaned))
}

/// Extract a doc comment from preceding sibling comment nodes.
fn extract_preceding_comment(node: Node, src: &[u8], lang: Lang) -> Option<String> {
    let mut current = node.prev_named_sibling()?;

    // Skip attribute/decorator nodes
    while let "attribute_item" | "decorator" | "annotation" | "attribute" | "attribute_list" =
        current.kind()
    {
        current = current.prev_named_sibling()?;
    }

    // Check if the node is a comment
    let is_comment = matches!(current.kind(), "line_comment" | "block_comment" | "comment");
    if !is_comment {
        return None;
    }

    let text = node_text(current, src);

    // Check for doc comment prefix based on language
    let is_doc = match lang {
        Lang::Rust => text.starts_with("///") || text.starts_with("/**"),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => text.starts_with("/**"),
        Lang::Go => text.starts_with("//"),
        Lang::Java | Lang::CSharp => text.starts_with("/**") || text.starts_with("///"),
        Lang::C | Lang::Cpp => text.starts_with("/**") || text.starts_with("///"),
        Lang::Ruby => text.starts_with("#"),
        Lang::Php => text.starts_with("/**"),
        Lang::Python => false, // handled by docstring extractor
    };

    if !is_doc {
        return None;
    }

    // For multi-line comments, also collect adjacent preceding comment siblings
    let mut lines = vec![text.to_string()];
    let mut prev = current.prev_named_sibling();
    while let Some(p) = prev {
        if !matches!(p.kind(), "line_comment" | "comment") {
            break;
        }
        let pt = node_text(p, src);
        // Must have the same doc prefix style
        let same_style = match lang {
            Lang::Rust => pt.starts_with("///"),
            Lang::Go => pt.starts_with("//"),
            Lang::Ruby => pt.starts_with("#"),
            _ => false,
        };
        if !same_style {
            break;
        }
        // Must be on the immediately preceding line
        if current.start_position().row != p.end_position().row + 1 {
            break;
        }
        lines.push(pt.to_string());
        current = p;
        prev = p.prev_named_sibling();
    }

    lines.reverse();
    let joined = lines.join("\n");
    let cleaned = strip_doc_prefix(&joined, lang);
    if cleaned.is_empty() {
        return None;
    }
    Some(truncate_doc(&cleaned))
}

/// Strip language-specific doc comment prefixes and normalize whitespace.
fn strip_doc_prefix(text: &str, lang: Lang) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let cleaned: Vec<String> = lines
        .iter()
        .map(|line| {
            let trimmed = line.trim();
            match lang {
                Lang::Rust => trimmed
                    .strip_prefix("///")
                    .or_else(|| trimmed.strip_prefix("/**"))
                    .or_else(|| trimmed.strip_prefix("* "))
                    .or_else(|| trimmed.strip_prefix("*"))
                    .or_else(|| trimmed.strip_suffix("*/"))
                    .unwrap_or(trimmed)
                    .trim()
                    .to_string(),
                Lang::TypeScript
                | Lang::Tsx
                | Lang::JavaScript
                | Lang::Java
                | Lang::CSharp
                | Lang::C
                | Lang::Cpp
                | Lang::Php => trimmed
                    .strip_prefix("/**")
                    .or_else(|| trimmed.strip_prefix("///"))
                    .or_else(|| trimmed.strip_prefix("* "))
                    .or_else(|| trimmed.strip_prefix("*"))
                    .or_else(|| trimmed.strip_suffix("*/"))
                    .unwrap_or(trimmed)
                    .trim()
                    .to_string(),
                Lang::Go => trimmed
                    .strip_prefix("//")
                    .unwrap_or(trimmed)
                    .trim()
                    .to_string(),
                Lang::Ruby => trimmed
                    .strip_prefix('#')
                    .unwrap_or(trimmed)
                    .trim()
                    .to_string(),
                Lang::Python => trimmed.to_string(),
            }
        })
        .filter(|s| !s.is_empty() && s != "/")
        .collect();
    cleaned.join(" ")
}

/// Truncate doc comment to MAX_DOC_COMMENT_LEN chars at a word boundary.
fn truncate_doc(s: &str) -> String {
    if s.len() <= MAX_DOC_COMMENT_LEN {
        return s.to_string();
    }
    // Find last space before the limit
    let truncated = &s[..MAX_DOC_COMMENT_LEN];
    if let Some(pos) = truncated.rfind(' ') {
        format!("{}...", &s[..pos])
    } else {
        format!("{}...", truncated)
    }
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

fn extract_rust(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    scope: Option<&str>,
) -> Option<Symbol> {
    match kind {
        "function_item" => {
            let name = field_text(node, "name", src)?;
            let sk = if scope.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            Some(make_symbol(name, sk, node, src, file, Lang::Rust, scope))
        }
        "function_signature_item" => {
            // Trait method signature: `fn foo(&self);`
            let name = field_text(node, "name", src)?;
            let sk = if scope.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            Some(make_symbol(name, sk, node, src, file, Lang::Rust, scope))
        }
        "struct_item" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Struct,
                node,
                src,
                file,
                Lang::Rust,
                scope,
            ))
        }
        "enum_item" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Enum,
                node,
                src,
                file,
                Lang::Rust,
                scope,
            ))
        }
        "trait_item" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Trait,
                node,
                src,
                file,
                Lang::Rust,
                scope,
            ))
        }
        "impl_item" => {
            // `impl Foo { ... }` or `impl Trait for Foo { ... }`
            // The "type" field holds the implementing type.
            let type_name = field_text(node, "type", src)?;
            // For `impl Trait for Type`, use "Type" as the scope name.
            let trait_name = field_text(node, "trait", src);
            let display_name = if let Some(tr) = trait_name {
                format!("{tr} for {type_name}")
            } else {
                type_name.to_string()
            };
            Some(make_symbol(
                &display_name,
                SymbolKind::Module, // impl block acts as a scope container
                node,
                src,
                file,
                Lang::Rust,
                scope,
            ))
        }
        "type_item" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::TypeAlias,
                node,
                src,
                file,
                Lang::Rust,
                scope,
            ))
        }
        "const_item" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Constant,
                node,
                src,
                file,
                Lang::Rust,
                scope,
            ))
        }
        "static_item" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Variable,
                node,
                src,
                file,
                Lang::Rust,
                scope,
            ))
        }
        "mod_item" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Module,
                node,
                src,
                file,
                Lang::Rust,
                scope,
            ))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

fn extract_python(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    scope: Option<&str>,
) -> Option<Symbol> {
    match kind {
        "function_definition" => {
            let name = field_text(node, "name", src)?;
            let sk = if scope.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            Some(make_symbol(name, sk, node, src, file, Lang::Python, scope))
        }
        "class_definition" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Class,
                node,
                src,
                file,
                Lang::Python,
                scope,
            ))
        }
        "decorated_definition" => {
            // A decorated definition wraps a function_definition or class_definition.
            // The inner definition will be visited when we recurse, so skip here.
            None
        }
        "assignment" => {
            // Module-level variable: `FOO = 42`
            if scope.is_none() {
                // Only capture simple name = value at module level
                if let Some(left) = node.child_by_field_name("left")
                    && left.kind() == "identifier"
                {
                    let name = node_text(left, src);
                    // Heuristic: ALL_CAPS = constant, otherwise variable
                    let is_const = name.chars().all(|c| c.is_uppercase() || c == '_');
                    let sk = if is_const {
                        SymbolKind::Constant
                    } else {
                        SymbolKind::Variable
                    };
                    return Some(make_symbol(name, sk, node, src, file, Lang::Python, scope));
                }
            }
            None
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// JavaScript
// ---------------------------------------------------------------------------

fn extract_javascript(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    scope: Option<&str>,
) -> Option<Symbol> {
    extract_js_common(node, kind, src, file, Lang::JavaScript, scope)
}

/// Shared extraction logic for JavaScript, TypeScript, and TSX.
fn extract_js_common(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    lang: Lang,
    scope: Option<&str>,
) -> Option<Symbol> {
    match kind {
        "function_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Function,
                node,
                src,
                file,
                lang,
                scope,
            ))
        }
        "generator_function_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Function,
                node,
                src,
                file,
                lang,
                scope,
            ))
        }
        "class_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Class,
                node,
                src,
                file,
                lang,
                scope,
            ))
        }
        "method_definition" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Method,
                node,
                src,
                file,
                lang,
                scope,
            ))
        }
        "variable_declaration" | "lexical_declaration" => {
            // `const foo = () => {}` or `let bar = function() {}`
            // Look for declarators with arrow_function or function values
            extract_js_var_decl(node, src, file, lang, scope)
        }
        _ => None,
    }
}

/// Extract a symbol from `const foo = ...` / `let bar = ...` declarations.
fn extract_js_var_decl(
    node: Node,
    src: &[u8],
    file: &str,
    lang: Lang,
    scope: Option<&str>,
) -> Option<Symbol> {
    // Look through variable_declarator children
    for i in 0..node.named_child_count() {
        let child = node.named_child(i as u32)?;
        if child.kind() == "variable_declarator" {
            let name_node = child.child_by_field_name("name")?;
            let name = node_text(name_node, src);
            let value = child.child_by_field_name("value");

            let sk = if let Some(val) = value {
                match val.kind() {
                    "arrow_function" | "function" | "function_expression" => SymbolKind::Function,
                    "class" => SymbolKind::Class,
                    _ => {
                        // Check if ALL_CAPS => constant
                        if is_upper_snake(name) {
                            SymbolKind::Constant
                        } else {
                            SymbolKind::Variable
                        }
                    }
                }
            } else if is_upper_snake(name) {
                SymbolKind::Constant
            } else {
                SymbolKind::Variable
            };

            return Some(make_symbol(name, sk, node, src, file, lang, scope));
        }
    }
    None
}

fn is_upper_snake(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_uppercase() || c == '_' || c.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// TypeScript / TSX
// ---------------------------------------------------------------------------

fn extract_typescript(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    lang: Lang,
    scope: Option<&str>,
) -> Option<Symbol> {
    // TypeScript has all JS constructs plus some extras.
    match kind {
        "interface_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Interface,
                node,
                src,
                file,
                lang,
                scope,
            ))
        }
        "type_alias_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::TypeAlias,
                node,
                src,
                file,
                lang,
                scope,
            ))
        }
        "enum_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Enum,
                node,
                src,
                file,
                lang,
                scope,
            ))
        }
        "module" => {
            // `namespace Foo { ... }` or `module Foo { ... }`
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Module,
                node,
                src,
                file,
                lang,
                scope,
            ))
        }
        // Interface/class member signatures — extracted so shallow mode can
        // query child signatures via `scope`.
        "method_signature" | "property_signature" => {
            let name = field_text(node, "name", src)?;
            let sk = if kind == "method_signature" {
                SymbolKind::Method
            } else {
                SymbolKind::Variable
            };
            Some(make_symbol(name, sk, node, src, file, lang, scope))
        }
        _ => extract_js_common(node, kind, src, file, lang, scope),
    }
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

fn extract_go(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    scope: Option<&str>,
) -> Option<Symbol> {
    match kind {
        "function_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Function,
                node,
                src,
                file,
                Lang::Go,
                scope,
            ))
        }
        "method_declaration" => {
            let name = field_text(node, "name", src)?;
            // Try to get the receiver type as scope
            let receiver_scope = node.child_by_field_name("receiver").and_then(|r| {
                // receiver is a parameter_list, get the type from its child
                r.named_child(0u32)
                    .and_then(|param| param.child_by_field_name("type"))
                    .map(|t| {
                        let text = node_text(t, src);
                        // Strip pointer prefix
                        text.trim_start_matches('*').to_string()
                    })
            });
            let scope_str = receiver_scope.as_deref().or(scope);
            Some(make_symbol(
                name,
                SymbolKind::Method,
                node,
                src,
                file,
                Lang::Go,
                scope_str,
            ))
        }
        "type_declaration" => {
            // `type Foo struct { ... }` or `type Bar interface { ... }`
            // Look at the type_spec children
            for i in 0..node.named_child_count() {
                if let Some(spec) = node.named_child(i as u32)
                    && spec.kind() == "type_spec"
                {
                    return extract_go_type_spec(spec, src, file, scope);
                }
            }
            None
        }
        "const_declaration" | "var_declaration" => {
            // Can have multiple specs
            let sk = if kind == "const_declaration" {
                SymbolKind::Constant
            } else {
                SymbolKind::Variable
            };
            for i in 0..node.named_child_count() {
                if let Some(spec) = node.named_child(i as u32)
                    && (spec.kind() == "const_spec" || spec.kind() == "var_spec")
                {
                    let name = field_text(spec, "name", src)
                        .or_else(|| spec.named_child(0u32).map(|n| node_text(n, src)));
                    if let Some(n) = name {
                        return Some(make_symbol(n, sk, node, src, file, Lang::Go, scope));
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_go_type_spec(spec: Node, src: &[u8], file: &str, scope: Option<&str>) -> Option<Symbol> {
    let name = field_text(spec, "name", src)?;
    let type_node = spec.child_by_field_name("type")?;
    let sk = match type_node.kind() {
        "struct_type" => SymbolKind::Struct,
        "interface_type" => SymbolKind::Interface,
        _ => SymbolKind::TypeAlias,
    };
    Some(make_symbol(name, sk, spec, src, file, Lang::Go, scope))
}

// ---------------------------------------------------------------------------
// Java
// ---------------------------------------------------------------------------

fn extract_java(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    scope: Option<&str>,
) -> Option<Symbol> {
    match kind {
        "class_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Class,
                node,
                src,
                file,
                Lang::Java,
                scope,
            ))
        }
        "interface_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Interface,
                node,
                src,
                file,
                Lang::Java,
                scope,
            ))
        }
        "enum_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Enum,
                node,
                src,
                file,
                Lang::Java,
                scope,
            ))
        }
        "method_declaration" | "constructor_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Method,
                node,
                src,
                file,
                Lang::Java,
                scope,
            ))
        }
        "field_declaration" => {
            // `static final int FOO = 42;`
            let declarator = node.named_child(node.named_child_count().saturating_sub(1) as u32)?;
            if declarator.kind() == "variable_declarator" {
                let name_node = declarator.child_by_field_name("name")?;
                let name = node_text(name_node, src);
                // Check for final keyword in modifiers to determine constant vs variable
                let text = node_text(node, src);
                let sk = if text.contains("final") {
                    SymbolKind::Constant
                } else {
                    SymbolKind::Variable
                };
                return Some(make_symbol(name, sk, node, src, file, Lang::Java, scope));
            }
            None
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// C
// ---------------------------------------------------------------------------

fn extract_c(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    scope: Option<&str>,
) -> Option<Symbol> {
    match kind {
        "function_definition" => {
            let declarator = node.child_by_field_name("declarator")?;
            let name = find_identifier_in_declarator(declarator, src)?;
            Some(make_symbol(
                name,
                SymbolKind::Function,
                node,
                src,
                file,
                Lang::C,
                scope,
            ))
        }
        "declaration" => {
            // Could be a function prototype, variable, or typedef
            let text = node_text(node, src);
            if text.starts_with("typedef") {
                // Extract the name from typedef
                if let Some(declarator) = node.child_by_field_name("declarator") {
                    let name = find_identifier_in_declarator(declarator, src)?;
                    return Some(make_symbol(
                        name,
                        SymbolKind::TypeAlias,
                        node,
                        src,
                        file,
                        Lang::C,
                        scope,
                    ));
                }
            }
            None
        }
        "struct_specifier" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Struct,
                node,
                src,
                file,
                Lang::C,
                scope,
            ))
        }
        "enum_specifier" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Enum,
                node,
                src,
                file,
                Lang::C,
                scope,
            ))
        }
        "preproc_def" => {
            // `#define FOO 42`
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Constant,
                node,
                src,
                file,
                Lang::C,
                scope,
            ))
        }
        _ => None,
    }
}

/// Walk a declarator tree to find the identifier name.
/// C declarators can be nested: `function_declarator` -> `identifier`,
/// or `pointer_declarator` -> `function_declarator` -> `identifier`.
fn find_identifier_in_declarator<'a>(node: Node<'a>, src: &'a [u8]) -> Option<&'a str> {
    if node.kind() == "identifier"
        || node.kind() == "type_identifier"
        || node.kind() == "field_identifier"
    {
        return Some(node_text(node, src));
    }
    // Try the "declarator" field first (common for function_declarator, pointer_declarator)
    if let Some(inner) = node.child_by_field_name("declarator") {
        return find_identifier_in_declarator(inner, src);
    }
    // Fallback: try "name" field
    if let Some(name) = node.child_by_field_name("name") {
        return Some(node_text(name, src));
    }
    // Walk named children
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i as u32)
            && let Some(name) = find_identifier_in_declarator(child, src)
        {
            return Some(name);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// C++
// ---------------------------------------------------------------------------

fn extract_cpp(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    scope: Option<&str>,
) -> Option<Symbol> {
    match kind {
        "function_definition" => {
            let declarator = node.child_by_field_name("declarator")?;
            let name = find_identifier_in_declarator(declarator, src)?;
            // Only treat as Method if inside a class/struct body
            // (field_declaration_list), not inside a namespace (declaration_list).
            let is_method = node
                .parent()
                .is_some_and(|p| p.kind() == "field_declaration_list");
            let sk = if is_method {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            Some(make_symbol(name, sk, node, src, file, Lang::Cpp, scope))
        }
        "class_specifier" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Class,
                node,
                src,
                file,
                Lang::Cpp,
                scope,
            ))
        }
        "struct_specifier" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Struct,
                node,
                src,
                file,
                Lang::Cpp,
                scope,
            ))
        }
        "enum_specifier" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Enum,
                node,
                src,
                file,
                Lang::Cpp,
                scope,
            ))
        }
        "namespace_definition" => {
            let name = field_text(node, "name", src).unwrap_or("anonymous");
            Some(make_symbol(
                name,
                SymbolKind::Module,
                node,
                src,
                file,
                Lang::Cpp,
                scope,
            ))
        }
        "declaration" => {
            let text = node_text(node, src);
            if (text.starts_with("typedef") || text.contains("using "))
                && let Some(declarator) = node.child_by_field_name("declarator")
                && let Some(name) = find_identifier_in_declarator(declarator, src)
            {
                return Some(make_symbol(
                    name,
                    SymbolKind::TypeAlias,
                    node,
                    src,
                    file,
                    Lang::Cpp,
                    scope,
                ));
            }
            None
        }
        "type_definition" | "alias_declaration" => {
            // `using Foo = Bar;` or `typedef ... Foo;`
            let name = field_text(node, "name", src).or_else(|| {
                node.child_by_field_name("declarator")
                    .and_then(|d| find_identifier_in_declarator(d, src))
            })?;
            Some(make_symbol(
                name,
                SymbolKind::TypeAlias,
                node,
                src,
                file,
                Lang::Cpp,
                scope,
            ))
        }
        "preproc_def" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Constant,
                node,
                src,
                file,
                Lang::Cpp,
                scope,
            ))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Ruby
// ---------------------------------------------------------------------------

fn extract_ruby(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    scope: Option<&str>,
) -> Option<Symbol> {
    match kind {
        "method" => {
            let name = field_text(node, "name", src)?;
            let sk = if scope.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            Some(make_symbol(name, sk, node, src, file, Lang::Ruby, scope))
        }
        "singleton_method" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Method,
                node,
                src,
                file,
                Lang::Ruby,
                scope,
            ))
        }
        "class" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Class,
                node,
                src,
                file,
                Lang::Ruby,
                scope,
            ))
        }
        "module" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Module,
                node,
                src,
                file,
                Lang::Ruby,
                scope,
            ))
        }
        "assignment" => {
            // Module-level constant (UPPER_CASE = ...)
            if let Some(left) = node.child_by_field_name("left")
                && left.kind() == "constant"
            {
                let name = node_text(left, src);
                return Some(make_symbol(
                    name,
                    SymbolKind::Constant,
                    node,
                    src,
                    file,
                    Lang::Ruby,
                    scope,
                ));
            }
            None
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// PHP
// ---------------------------------------------------------------------------

fn extract_php(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    scope: Option<&str>,
) -> Option<Symbol> {
    match kind {
        "function_definition" => {
            let name = field_text(node, "name", src)?;
            let sk = if scope.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            Some(make_symbol(name, sk, node, src, file, Lang::Php, scope))
        }
        "method_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Method,
                node,
                src,
                file,
                Lang::Php,
                scope,
            ))
        }
        "class_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Class,
                node,
                src,
                file,
                Lang::Php,
                scope,
            ))
        }
        "interface_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Interface,
                node,
                src,
                file,
                Lang::Php,
                scope,
            ))
        }
        "trait_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Trait,
                node,
                src,
                file,
                Lang::Php,
                scope,
            ))
        }
        "enum_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Enum,
                node,
                src,
                file,
                Lang::Php,
                scope,
            ))
        }
        "const_declaration" => {
            // `const FOO = 42;`
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i as u32)
                    && child.kind() == "const_element"
                    && let Some(name_node) = child.child_by_field_name("name")
                {
                    let name = node_text(name_node, src);
                    return Some(make_symbol(
                        name,
                        SymbolKind::Constant,
                        node,
                        src,
                        file,
                        Lang::Php,
                        scope,
                    ));
                }
            }
            None
        }
        "namespace_definition" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Module,
                node,
                src,
                file,
                Lang::Php,
                scope,
            ))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// C#
// ---------------------------------------------------------------------------

fn extract_csharp(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    scope: Option<&str>,
) -> Option<Symbol> {
    match kind {
        "class_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Class,
                node,
                src,
                file,
                Lang::CSharp,
                scope,
            ))
        }
        "record_declaration" => {
            let name = field_text(node, "name", src)?;
            // record class → Class, record struct → Struct
            let text = node_text(node, src);
            let sk = if text.contains("record struct") {
                SymbolKind::Struct
            } else {
                SymbolKind::Class
            };
            Some(make_symbol(name, sk, node, src, file, Lang::CSharp, scope))
        }
        "struct_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Struct,
                node,
                src,
                file,
                Lang::CSharp,
                scope,
            ))
        }
        "interface_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Interface,
                node,
                src,
                file,
                Lang::CSharp,
                scope,
            ))
        }
        "enum_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Enum,
                node,
                src,
                file,
                Lang::CSharp,
                scope,
            ))
        }
        "namespace_declaration" | "file_scoped_namespace_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Module,
                node,
                src,
                file,
                Lang::CSharp,
                scope,
            ))
        }
        "delegate_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::TypeAlias,
                node,
                src,
                file,
                Lang::CSharp,
                scope,
            ))
        }
        "method_declaration" | "constructor_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Method,
                node,
                src,
                file,
                Lang::CSharp,
                scope,
            ))
        }
        "property_declaration" => {
            let name = field_text(node, "name", src)?;
            Some(make_symbol(
                name,
                SymbolKind::Method,
                node,
                src,
                file,
                Lang::CSharp,
                scope,
            ))
        }
        "field_declaration" => {
            // `public const int MAX = 100;` or `private int _count;`
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i as u32)
                    && child.kind() == "variable_declaration"
                {
                    for j in 0..child.named_child_count() {
                        if let Some(decl) = child.named_child(j as u32)
                            && decl.kind() == "variable_declarator"
                            && let Some(name_node) = decl.child_by_field_name("name")
                        {
                            let name = node_text(name_node, src);
                            let text = node_text(node, src);
                            let sk = if text.contains("const") || text.contains("readonly") {
                                SymbolKind::Constant
                            } else {
                                SymbolKind::Variable
                            };
                            return Some(make_symbol(
                                name,
                                sk,
                                node,
                                src,
                                file,
                                Lang::CSharp,
                                scope,
                            ));
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

// ===========================================================================
// Confidence scoring
// ===========================================================================

/// Compute a confidence score for a reference based on available evidence.
///
/// Scoring rules (highest applicable wins):
/// - Import reference kind -> 0.95
/// - Name appears in the file's import list -> 0.95
/// - Same-file definition exists (name matches a symbol in the same file) -> 0.85
/// - Same-scope (caller shares scope with a definition of the referenced name) -> 0.80
/// - Cross-file name match (default) -> 0.50
pub fn compute_confidence(reference: &Reference, symbols: &[Symbol], imports: &[String]) -> f64 {
    // Rule 1: Import reference kind.
    if reference.kind == ReferenceKind::Import {
        return 0.95;
    }

    // Rule 2: Name is in the file's import list (import-resolved).
    // Pre-build suffix strings once to avoid repeated allocations per import.
    let suffix_colon = format!("::{}", reference.name);
    let suffix_slash = format!("/{}", reference.name);
    let suffix_dot = format!(".{}", reference.name);
    if imports.iter().any(|imp| {
        // Check if the import path ends with the reference name
        // e.g. "std::collections::HashMap" (::), "path/to/module" (/), or "module.Symbol" (.)
        imp == &reference.name
            || imp.ends_with(&suffix_colon)
            || imp.ends_with(&suffix_slash)
            || imp.ends_with(&suffix_dot)
    }) {
        return 0.95;
    }

    // Rule 3: Same-file definition exists.
    let same_file_def = symbols
        .iter()
        .any(|s| s.name == reference.name && s.file == reference.file);
    if same_file_def {
        return 0.85;
    }

    // Rule 4: Same-scope — the caller and a definition of the referenced name
    // share the same scope (e.g. both are methods of the same class).
    if let Some(ref caller_name) = reference.caller_name {
        // Find the scope of the caller.
        let caller_scope = symbols
            .iter()
            .find(|s| s.name == *caller_name && s.file == reference.file)
            .and_then(|s| s.scope.as_deref());

        if let Some(scope) = caller_scope {
            let same_scope_def = symbols
                .iter()
                .any(|s| s.name == reference.name && s.scope.as_deref() == Some(scope));
            if same_scope_def {
                return 0.80;
            }
        }
    }

    // Default: cross-file name match.
    0.50
}

// ===========================================================================
// Reference extraction
// ===========================================================================

/// Extract references (function calls, type annotations, import statements)
/// from a parsed syntax tree.
///
/// Walks the entire tree and collects references with their source context.
pub fn extract_references(tree: &Tree, source: &str, file: &str, lang: Lang) -> Vec<Reference> {
    let src = source.as_bytes();
    let source_lines: Vec<&str> = source.lines().collect();
    let mut refs = Vec::new();
    walk_refs(tree.root_node(), src, file, lang, &source_lines, &mut refs);
    refs
}

/// Recursively walk a node tree collecting references.
fn walk_refs(
    node: Node,
    src: &[u8],
    file: &str,
    lang: Lang,
    source_lines: &[&str],
    refs: &mut Vec<Reference>,
) {
    let kind = node.kind();

    // Check for call expressions
    if let Some(mut r) = match_call_ref(node, kind, src, file, lang, source_lines) {
        r.caller_name = find_enclosing_function(node, src, lang);
        refs.push(r);
    }

    // Check for type references
    if let Some(r) = match_type_ref(node, kind, src, file, lang, source_lines) {
        refs.push(r);
    }

    // Check for import references
    if let Some(r) = match_import_ref(node, kind, src, file, lang, source_lines) {
        refs.push(r);
    }

    // Recurse into children
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            walk_refs(child, src, file, lang, source_lines, refs);
        }
    }
}

/// Get the source line at a given 0-based row.
fn get_context_line(source_lines: &[&str], row: usize) -> String {
    source_lines.get(row).unwrap_or(&"").trim().to_string()
}

/// Build a `Reference` from a node.
fn make_ref(
    name: &str,
    kind: ReferenceKind,
    node: Node,
    file: &str,
    source_lines: &[&str],
) -> Reference {
    let row = node.start_position().row;
    Reference {
        name: name.to_string(),
        kind,
        file: file.to_string(),
        line: row + 1,
        col: node.start_position().column,
        context: get_context_line(source_lines, row),
        caller_name: None,
        confidence: 0.5,
    }
}

// ---------------------------------------------------------------------------
// Enclosing function detection (for call graph caller_name)
// ---------------------------------------------------------------------------

/// Returns `true` if the tree-sitter node kind represents a function or method
/// definition in the given language.
fn is_function_node(kind: &str, lang: Lang) -> bool {
    match lang {
        Lang::Rust => kind == "function_item",
        Lang::Python => kind == "function_definition",
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => matches!(
            kind,
            "function_declaration"
                | "generator_function_declaration"
                | "method_definition"
                | "arrow_function"
        ),
        Lang::Go => matches!(kind, "function_declaration" | "method_declaration"),
        Lang::Java => matches!(kind, "method_declaration" | "constructor_declaration"),
        Lang::C | Lang::Cpp => kind == "function_definition",
        Lang::Ruby => matches!(kind, "method" | "singleton_method"),
        Lang::Php => matches!(kind, "function_definition" | "method_declaration"),
        Lang::CSharp => matches!(kind, "method_declaration" | "constructor_declaration"),
    }
}

/// Extract the function/method name from a function node.
///
/// Uses `child_by_field_name("name")` for most languages.  For JS/TS
/// `arrow_function`, walks the parent to find a `variable_declarator` name.
/// Returns `None` for anonymous functions.
fn function_name_from_node(node: Node, src: &[u8], lang: Lang) -> Option<String> {
    let kind = node.kind();

    // Arrow functions don't have a "name" field — look at the parent for
    // `variable_declarator` (e.g. `const foo = () => { ... }`).
    if matches!(lang, Lang::JavaScript | Lang::TypeScript | Lang::Tsx) && kind == "arrow_function" {
        let parent = node.parent()?;
        if parent.kind() == "variable_declarator" {
            let name_node = parent.child_by_field_name("name")?;
            let name = name_node.utf8_text(src).ok()?;
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
        return None;
    }

    // C/C++ function_definition uses a `declarator` field (which may be
    // nested: pointer_declarator → function_declarator → identifier).
    if matches!(lang, Lang::C | Lang::Cpp) && kind == "function_definition" {
        let declarator = node.child_by_field_name("declarator")?;
        return find_identifier_in_declarator(declarator, src).map(|s| s.to_string());
    }

    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(src).ok()?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// Walk `node.parent()` upward until a function/method node is found, then
/// return its name.  Returns `None` for file-scope calls.
fn find_enclosing_function(node: Node, src: &[u8], lang: Lang) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if is_function_node(parent.kind(), lang) {
            return function_name_from_node(parent, src, lang);
        }
        current = parent.parent();
    }
    None
}

// ---------------------------------------------------------------------------
// Call reference matching
// ---------------------------------------------------------------------------

/// Try to extract a call reference from a node.
fn match_call_ref(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    lang: Lang,
    source_lines: &[&str],
) -> Option<Reference> {
    match lang {
        Lang::Rust => match_rust_call(node, kind, src, file, source_lines),
        Lang::Python => match_python_call(node, kind, src, file, source_lines),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            match_js_call(node, kind, src, file, source_lines)
        }
        Lang::Go => match_go_call(node, kind, src, file, source_lines),
        Lang::Java => match_java_call(node, kind, src, file, source_lines),
        Lang::C | Lang::Cpp => match_c_call(node, kind, src, file, source_lines),
        Lang::Ruby => match_ruby_call(node, kind, src, file, source_lines),
        Lang::Php => match_php_call(node, kind, src, file, source_lines),
        Lang::CSharp => match_csharp_call(node, kind, src, file, source_lines),
    }
}

fn match_rust_call(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    source_lines: &[&str],
) -> Option<Reference> {
    if kind != "call_expression" {
        return None;
    }
    let func = node.child_by_field_name("function")?;
    let name = extract_call_name(func, src);
    if name.is_empty() {
        return None;
    }
    Some(make_ref(
        &name,
        ReferenceKind::Call,
        node,
        file,
        source_lines,
    ))
}

fn match_python_call(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    source_lines: &[&str],
) -> Option<Reference> {
    if kind != "call" {
        return None;
    }
    let func = node.child_by_field_name("function")?;
    let name = extract_call_name(func, src);
    if name.is_empty() {
        return None;
    }
    Some(make_ref(
        &name,
        ReferenceKind::Call,
        node,
        file,
        source_lines,
    ))
}

fn match_js_call(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    source_lines: &[&str],
) -> Option<Reference> {
    if kind != "call_expression" {
        return None;
    }
    let func = node.child_by_field_name("function")?;
    let name = extract_call_name(func, src);
    if name.is_empty() {
        return None;
    }
    Some(make_ref(
        &name,
        ReferenceKind::Call,
        node,
        file,
        source_lines,
    ))
}

fn match_go_call(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    source_lines: &[&str],
) -> Option<Reference> {
    if kind != "call_expression" {
        return None;
    }
    let func = node.child_by_field_name("function")?;
    let name = extract_call_name(func, src);
    if name.is_empty() {
        return None;
    }
    Some(make_ref(
        &name,
        ReferenceKind::Call,
        node,
        file,
        source_lines,
    ))
}

fn match_java_call(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    source_lines: &[&str],
) -> Option<Reference> {
    if kind != "method_invocation" {
        return None;
    }
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, src);
    if name.is_empty() {
        return None;
    }
    // Include the object if present: obj.method
    let full_name = if let Some(obj) = node.child_by_field_name("object") {
        let obj_text = node_text(obj, src);
        format!("{obj_text}.{name}")
    } else {
        name.to_string()
    };
    Some(make_ref(
        &full_name,
        ReferenceKind::Call,
        node,
        file,
        source_lines,
    ))
}

fn match_c_call(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    source_lines: &[&str],
) -> Option<Reference> {
    if kind != "call_expression" {
        return None;
    }
    let func = node.child_by_field_name("function")?;
    let name = extract_call_name(func, src);
    if name.is_empty() {
        return None;
    }
    Some(make_ref(
        &name,
        ReferenceKind::Call,
        node,
        file,
        source_lines,
    ))
}

fn match_ruby_call(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    source_lines: &[&str],
) -> Option<Reference> {
    // Ruby uses "call" for method calls with explicit receiver
    // and "method_call" or just bare identifiers with arguments
    match kind {
        "call" => {
            let method = node.child_by_field_name("method")?;
            let name = node_text(method, src);
            if name.is_empty() {
                return None;
            }
            // Include receiver if present
            let full_name = if let Some(recv) = node.child_by_field_name("receiver") {
                let recv_text = node_text(recv, src);
                format!("{recv_text}.{name}")
            } else {
                name.to_string()
            };
            Some(make_ref(
                &full_name,
                ReferenceKind::Call,
                node,
                file,
                source_lines,
            ))
        }
        "method_call" => {
            let method = node.child_by_field_name("method")?;
            let name = node_text(method, src);
            if name.is_empty() {
                return None;
            }
            Some(make_ref(
                name,
                ReferenceKind::Call,
                node,
                file,
                source_lines,
            ))
        }
        _ => None,
    }
}

fn match_php_call(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    source_lines: &[&str],
) -> Option<Reference> {
    match kind {
        "function_call_expression" => {
            let func = node.child_by_field_name("function")?;
            let name = node_text(func, src);
            if name.is_empty() {
                return None;
            }
            Some(make_ref(
                name,
                ReferenceKind::Call,
                node,
                file,
                source_lines,
            ))
        }
        "member_call_expression" => {
            let name_node = node.child_by_field_name("name")?;
            let name = node_text(name_node, src);
            if name.is_empty() {
                return None;
            }
            Some(make_ref(
                name,
                ReferenceKind::Call,
                node,
                file,
                source_lines,
            ))
        }
        "scoped_call_expression" => {
            let name_node = node.child_by_field_name("name")?;
            let name = node_text(name_node, src);
            if name.is_empty() {
                return None;
            }
            Some(make_ref(
                name,
                ReferenceKind::Call,
                node,
                file,
                source_lines,
            ))
        }
        _ => None,
    }
}

fn match_csharp_call(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    source_lines: &[&str],
) -> Option<Reference> {
    if kind != "invocation_expression" {
        return None;
    }
    let func = node.child_by_field_name("function")?;
    match func.kind() {
        "member_access_expression" => {
            let name_node = func.child_by_field_name("name")?;
            let name = node_text(name_node, src);
            if name.is_empty() {
                return None;
            }
            Some(make_ref(
                name,
                ReferenceKind::Call,
                node,
                file,
                source_lines,
            ))
        }
        _ => {
            let name = node_text(func, src);
            if name.is_empty() {
                return None;
            }
            // Take last segment for dotted names
            let short = name.rsplit_once('.').map(|(_, last)| last).unwrap_or(name);
            Some(make_ref(
                short,
                ReferenceKind::Call,
                node,
                file,
                source_lines,
            ))
        }
    }
}

/// Extract the function/method name from a call target node.
///
/// Handles `identifier`, `member_expression` (a.b), `field_expression`,
/// `scoped_identifier` (a::b), etc.
fn extract_call_name(node: Node, src: &[u8]) -> String {
    match node.kind() {
        "identifier" | "field_identifier" => node_text(node, src).to_string(),
        // `a.b.c` -> just the method name `c` (for member_expression, field_expression)
        "member_expression" | "field_expression" | "attribute" => {
            if let Some(prop) = node
                .child_by_field_name("property")
                .or_else(|| node.child_by_field_name("field"))
                .or_else(|| node.child_by_field_name("attribute"))
            {
                node_text(prop, src).to_string()
            } else {
                node_text(node, src).to_string()
            }
        }
        // Rust: `a::b::c` -> last segment
        "scoped_identifier" => {
            if let Some(name) = node.child_by_field_name("name") {
                node_text(name, src).to_string()
            } else {
                node_text(node, src).to_string()
            }
        }
        // Go: selector_expression `pkg.Func`
        "selector_expression" => {
            if let Some(field) = node.child_by_field_name("field") {
                node_text(field, src).to_string()
            } else {
                node_text(node, src).to_string()
            }
        }
        // Fallback: use the whole text
        _ => {
            let text = node_text(node, src);
            // Try to get just the last segment for dotted names
            text.rsplit_once('.')
                .or_else(|| text.rsplit_once("::"))
                .map(|(_, last)| last)
                .unwrap_or(text)
                .to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Type reference matching
// ---------------------------------------------------------------------------

/// Try to extract a type reference from a node.
fn match_type_ref(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    lang: Lang,
    source_lines: &[&str],
) -> Option<Reference> {
    match lang {
        Lang::Rust => match kind {
            "type_identifier" => {
                // Only if parent is a type context (not a definition site)
                let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");
                // Skip definition sites
                if matches!(
                    parent_kind,
                    "struct_item" | "enum_item" | "trait_item" | "type_item"
                ) {
                    return None;
                }
                let name = node_text(node, src);
                if name.is_empty() {
                    return None;
                }
                Some(make_ref(
                    name,
                    ReferenceKind::Type,
                    node,
                    file,
                    source_lines,
                ))
            }
            _ => None,
        },
        Lang::Python => match kind {
            "type" => {
                let name = node_text(node, src).trim().to_string();
                if name.is_empty() {
                    return None;
                }
                // Python type annotations: `x: int`, `def foo() -> str:`
                Some(make_ref(
                    &name,
                    ReferenceKind::Type,
                    node,
                    file,
                    source_lines,
                ))
            }
            _ => None,
        },
        Lang::TypeScript | Lang::Tsx => match kind {
            "type_identifier" => {
                let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");
                // Skip definition sites
                if matches!(
                    parent_kind,
                    "interface_declaration"
                        | "type_alias_declaration"
                        | "enum_declaration"
                        | "class_declaration"
                ) {
                    return None;
                }
                let name = node_text(node, src);
                if name.is_empty() {
                    return None;
                }
                Some(make_ref(
                    name,
                    ReferenceKind::Type,
                    node,
                    file,
                    source_lines,
                ))
            }
            _ => None,
        },
        Lang::JavaScript => None, // JS has no type annotations
        Lang::Go => match kind {
            "type_identifier" => {
                let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");
                if matches!(parent_kind, "type_spec") {
                    return None;
                }
                let name = node_text(node, src);
                if name.is_empty() || is_go_builtin_type(name) {
                    return None;
                }
                Some(make_ref(
                    name,
                    ReferenceKind::Type,
                    node,
                    file,
                    source_lines,
                ))
            }
            _ => None,
        },
        Lang::Java => match kind {
            "type_identifier" => {
                let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");
                if matches!(
                    parent_kind,
                    "class_declaration" | "interface_declaration" | "enum_declaration"
                ) {
                    return None;
                }
                let name = node_text(node, src);
                if name.is_empty() {
                    return None;
                }
                Some(make_ref(
                    name,
                    ReferenceKind::Type,
                    node,
                    file,
                    source_lines,
                ))
            }
            _ => None,
        },
        Lang::C | Lang::Cpp => match kind {
            "type_identifier" => {
                let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");
                if matches!(
                    parent_kind,
                    "struct_specifier" | "class_specifier" | "enum_specifier" | "type_definition"
                ) {
                    return None;
                }
                let name = node_text(node, src);
                if name.is_empty() {
                    return None;
                }
                Some(make_ref(
                    name,
                    ReferenceKind::Type,
                    node,
                    file,
                    source_lines,
                ))
            }
            _ => None,
        },
        Lang::Ruby => None, // Ruby is dynamically typed, no type annotations
        Lang::Php => match kind {
            "named_type" => {
                let name = node_text(node, src);
                if name.is_empty() {
                    return None;
                }
                Some(make_ref(
                    name,
                    ReferenceKind::Type,
                    node,
                    file,
                    source_lines,
                ))
            }
            _ => None,
        },
        Lang::CSharp => match kind {
            "identifier" => {
                let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");
                // Type contexts: base types, type arguments, parameter types, return types, etc.
                if !matches!(
                    parent_kind,
                    "base_list"
                        | "type_argument_list"
                        | "type_constraint"
                        | "object_creation_expression"
                        | "generic_name"
                        | "nullable_type"
                        | "array_type"
                ) {
                    return None;
                }
                let name = node_text(node, src);
                if name.is_empty() {
                    return None;
                }
                Some(make_ref(
                    name,
                    ReferenceKind::Type,
                    node,
                    file,
                    source_lines,
                ))
            }
            _ => None,
        },
    }
}

fn is_go_builtin_type(name: &str) -> bool {
    matches!(
        name,
        "bool"
            | "byte"
            | "complex64"
            | "complex128"
            | "error"
            | "float32"
            | "float64"
            | "int"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "rune"
            | "string"
            | "uint"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uintptr"
    )
}

// ---------------------------------------------------------------------------
// Import reference matching
// ---------------------------------------------------------------------------

/// Try to extract an import reference from a node.
fn match_import_ref(
    node: Node,
    kind: &str,
    src: &[u8],
    file: &str,
    lang: Lang,
    source_lines: &[&str],
) -> Option<Reference> {
    match lang {
        Lang::Rust => {
            if kind != "use_declaration" {
                return None;
            }
            let arg = node.child_by_field_name("argument")?;
            let name = node_text(arg, src).to_string();
            Some(make_ref(
                &name,
                ReferenceKind::Import,
                node,
                file,
                source_lines,
            ))
        }
        Lang::Python => match kind {
            "import_statement" | "import_from_statement" => {
                let name = node_text(node, src).trim().to_string();
                Some(make_ref(
                    &name,
                    ReferenceKind::Import,
                    node,
                    file,
                    source_lines,
                ))
            }
            _ => None,
        },
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            if kind != "import_statement" {
                return None;
            }
            let name = node_text(node, src).trim().to_string();
            Some(make_ref(
                &name,
                ReferenceKind::Import,
                node,
                file,
                source_lines,
            ))
        }
        Lang::Go => {
            if kind != "import_spec" {
                return None;
            }
            let path = node.child_by_field_name("path")?;
            let name = node_text(path, src).trim_matches('"').to_string();
            Some(make_ref(
                &name,
                ReferenceKind::Import,
                node,
                file,
                source_lines,
            ))
        }
        Lang::Java => {
            if kind != "import_declaration" {
                return None;
            }
            // The text minus the "import " prefix and ";" suffix
            let text = node_text(node, src).trim().to_string();
            Some(make_ref(
                &text,
                ReferenceKind::Import,
                node,
                file,
                source_lines,
            ))
        }
        Lang::C | Lang::Cpp => {
            if kind != "preproc_include" {
                return None;
            }
            let path = node.child_by_field_name("path")?;
            let name = node_text(path, src)
                .trim_matches(|c| c == '"' || c == '<' || c == '>')
                .to_string();
            Some(make_ref(
                &name,
                ReferenceKind::Import,
                node,
                file,
                source_lines,
            ))
        }
        Lang::Ruby => {
            match kind {
                "call" | "method_call" => {
                    // require 'foo' or require_relative 'foo'
                    let method = node
                        .child_by_field_name("method")
                        .map(|n| node_text(n, src))
                        .unwrap_or("");
                    if !matches!(method, "require" | "require_relative") {
                        return None;
                    }
                    let args = node.child_by_field_name("arguments")?;
                    let arg = args.named_child(0u32)?;
                    let name = node_text(arg, src)
                        .trim_matches(|c| c == '\'' || c == '"')
                        .to_string();
                    Some(make_ref(
                        &name,
                        ReferenceKind::Import,
                        node,
                        file,
                        source_lines,
                    ))
                }
                _ => None,
            }
        }
        Lang::Php => {
            match kind {
                "named_label_statement" => None,
                _ => {
                    // PHP: include, require, include_once, require_once
                    if !matches!(
                        kind,
                        "include_expression"
                            | "include_once_expression"
                            | "require_expression"
                            | "require_once_expression"
                    ) {
                        return None;
                    }
                    // The argument is a string literal child
                    let text = node_text(node, src).trim().to_string();
                    Some(make_ref(
                        &text,
                        ReferenceKind::Import,
                        node,
                        file,
                        source_lines,
                    ))
                }
            }
        }
        Lang::CSharp => {
            if kind != "using_directive" {
                return None;
            }
            let text = node_text(node, src).trim().to_string();
            Some(make_ref(
                &text,
                ReferenceKind::Import,
                node,
                file,
                source_lines,
            ))
        }
    }
}

// ===========================================================================
// Import/export extraction for file dependency graph
// ===========================================================================

/// Extract import and export data from a parsed syntax tree.
///
/// Returns a [`FileImports`] with the list of imported module paths and
/// exported symbol names for dependency graph construction.
pub fn extract_imports(tree: &Tree, source: &str, file: &str, lang: Lang) -> FileImports {
    let src = source.as_bytes();
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    walk_imports(tree.root_node(), src, lang, &mut imports, &mut exports);
    FileImports {
        file: file.to_string(),
        imports,
        exports,
    }
}

/// Recursively walk the tree collecting import paths and export names.
fn walk_imports(
    node: Node,
    src: &[u8],
    lang: Lang,
    imports: &mut Vec<String>,
    exports: &mut Vec<String>,
) {
    let kind = node.kind();

    match lang {
        Lang::Rust => {
            if kind == "use_declaration"
                && let Some(arg) = node.child_by_field_name("argument")
            {
                imports.push(node_text(arg, src).to_string());
            }
            // Rust pub items are exports (simplified: just look for `pub` visibility)
            if kind == "visibility_modifier"
                && node_text(node, src).starts_with("pub")
                && let Some(parent) = node.parent()
            {
                let export_name = match parent.kind() {
                    "function_item" | "struct_item" | "enum_item" | "trait_item" | "type_item"
                    | "const_item" | "static_item" | "mod_item" => {
                        field_text(parent, "name", src).map(|s| s.to_string())
                    }
                    _ => None,
                };
                if let Some(name) = export_name {
                    exports.push(name);
                }
            }
        }
        Lang::Python => {
            match kind {
                "import_statement" => {
                    // import foo, bar
                    for i in 0..node.named_child_count() {
                        if let Some(child) = node.named_child(i as u32)
                            && (child.kind() == "dotted_name" || child.kind() == "aliased_import")
                        {
                            let name_node = if child.kind() == "aliased_import" {
                                child.child_by_field_name("name")
                            } else {
                                Some(child)
                            };
                            if let Some(n) = name_node {
                                imports.push(node_text(n, src).to_string());
                            }
                        }
                    }
                }
                "import_from_statement" => {
                    if let Some(module) = node.child_by_field_name("module_name") {
                        imports.push(node_text(module, src).to_string());
                    }
                }
                _ => {}
            }
        }
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            if kind == "import_statement"
                && let Some(source_node) = node.child_by_field_name("source")
            {
                let path = node_text(source_node, src)
                    .trim_matches(|c| c == '\'' || c == '"')
                    .to_string();
                imports.push(path);
            }
            // Export statements
            if kind == "export_statement" {
                // `export function foo() {}` or `export { foo, bar }`
                // Try to get the declaration's name
                if let Some(decl) = node.child_by_field_name("declaration")
                    && let Some(name) = field_text(decl, "name", src)
                {
                    exports.push(name.to_string());
                }
                // `export { foo, bar }` - look for export_clause
                for i in 0..node.named_child_count() {
                    if let Some(child) = node.named_child(i as u32)
                        && child.kind() == "export_clause"
                    {
                        for j in 0..child.named_child_count() {
                            if let Some(spec) = child.named_child(j as u32)
                                && spec.kind() == "export_specifier"
                                && let Some(name) = spec.child_by_field_name("name")
                            {
                                exports.push(node_text(name, src).to_string());
                            }
                        }
                    }
                }
                // `export default` - add "default"
                let text = node_text(node, src);
                if text.contains("export default") {
                    exports.push("default".to_string());
                }
            }
        }
        Lang::Go => {
            if kind == "import_spec"
                && let Some(path) = node.child_by_field_name("path")
            {
                imports.push(node_text(path, src).trim_matches('"').to_string());
            }
            // Go exports: capitalized top-level names (handled by convention,
            // we capture them for completeness)
            if matches!(
                kind,
                "function_declaration"
                    | "type_declaration"
                    | "const_declaration"
                    | "var_declaration"
            ) && let Some(name) = field_text(node, "name", src)
                && name.starts_with(|c: char| c.is_uppercase())
            {
                exports.push(name.to_string());
            }
        }
        Lang::Java => {
            if kind == "import_declaration" {
                // Extract the imported path (skip "import " and ";")
                for i in 0..node.named_child_count() {
                    if let Some(child) = node.named_child(i as u32)
                        && child.kind() == "scoped_identifier"
                    {
                        imports.push(node_text(child, src).to_string());
                    }
                }
            }
        }
        Lang::C | Lang::Cpp => {
            if kind == "preproc_include"
                && let Some(path) = node.child_by_field_name("path")
            {
                let text = node_text(path, src)
                    .trim_matches(|c| c == '"' || c == '<' || c == '>')
                    .to_string();
                imports.push(text);
            }
        }
        Lang::Ruby => {
            if matches!(kind, "call" | "method_call") {
                let method = node
                    .child_by_field_name("method")
                    .map(|n| node_text(n, src))
                    .unwrap_or("");
                if matches!(method, "require" | "require_relative")
                    && let Some(args) = node.child_by_field_name("arguments")
                    && let Some(arg) = args.named_child(0u32)
                {
                    let path = node_text(arg, src)
                        .trim_matches(|c| c == '\'' || c == '"')
                        .to_string();
                    imports.push(path);
                }
            }
        }
        Lang::Php => {
            if matches!(
                kind,
                "include_expression"
                    | "include_once_expression"
                    | "require_expression"
                    | "require_once_expression"
            ) {
                // Get the string argument
                for i in 0..node.named_child_count() {
                    if let Some(child) = node.named_child(i as u32)
                        && child.kind() == "string"
                    {
                        let path = node_text(child, src)
                            .trim_matches(|c| c == '\'' || c == '"')
                            .to_string();
                        imports.push(path);
                    }
                }
            }
            // PHP namespace use statements
            if kind == "namespace_use_declaration" {
                for i in 0..node.named_child_count() {
                    if let Some(child) = node.named_child(i as u32)
                        && child.kind() == "namespace_use_clause"
                    {
                        imports.push(node_text(child, src).to_string());
                    }
                }
            }
        }
        Lang::CSharp => {
            if kind == "using_directive" {
                // `using System.Collections.Generic;` → extract the namespace
                for i in 0..node.named_child_count() {
                    if let Some(child) = node.named_child(i as u32)
                        && matches!(child.kind(), "qualified_name" | "identifier")
                    {
                        imports.push(node_text(child, src).to_string());
                    }
                }
            }
            // C# public types at namespace level are effectively exports
            if matches!(
                kind,
                "class_declaration"
                    | "struct_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "delegate_declaration"
                    | "record_declaration"
            ) {
                let text = node_text(node, src);
                if text.contains("public")
                    && let Some(name) = field_text(node, "name", src)
                {
                    exports.push(name.to_string());
                }
            }
        }
    }

    // Recurse into children
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            walk_imports(child, src, lang, imports, exports);
        }
    }
}

// ===========================================================================
// Type hierarchy edge extraction
// ===========================================================================

/// Extract type hierarchy edges (extends/implements) from a parsed syntax tree.
///
/// Returns a list of [`RawTypeEdge`] with unresolved names. The pipeline
/// resolves these to symbol IDs before inserting into the `type_edges` table.
///
/// C and Go are skipped (no class-based inheritance).
pub fn extract_type_edges(tree: &Tree, source: &str, _file: &str, lang: Lang) -> Vec<RawTypeEdge> {
    // C and Go have no class-based inheritance.
    if matches!(lang, Lang::C | Lang::Go) {
        return Vec::new();
    }

    let src = source.as_bytes();
    let mut edges = Vec::new();
    walk_type_edges(tree.root_node(), src, lang, &mut edges);
    edges
}

/// Recursively walk the tree collecting type hierarchy edges.
fn walk_type_edges(node: Node, src: &[u8], lang: Lang, edges: &mut Vec<RawTypeEdge>) {
    let kind = node.kind();

    match lang {
        Lang::TypeScript | Lang::Tsx => {
            if kind == "class_declaration"
                && let Some(class_name) = field_text(node, "name", src)
            {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i as u32)
                        && child.kind() == "class_heritage"
                    {
                        extract_ts_heritage(child, src, class_name, edges);
                    }
                }
            }
        }
        Lang::JavaScript => {
            if (kind == "class_declaration" || kind == "class")
                && let Some(class_name) = field_text(node, "name", src)
            {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i as u32)
                        && child.kind() == "class_heritage"
                    {
                        // In JS, class_heritage children: extends keyword, then identifier.
                        for j in 0..child.child_count() {
                            if let Some(gchild) = child.child(j as u32)
                                && gchild.kind() == "identifier"
                            {
                                let parent = node_text(gchild, src);
                                if !parent.is_empty() {
                                    edges.push(RawTypeEdge {
                                        child_name: class_name.to_string(),
                                        parent_name: parent.to_string(),
                                        relationship: "extends".to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        Lang::Python => {
            if kind == "class_definition"
                && let Some(class_name) = field_text(node, "name", src)
                && let Some(superclasses) = node.child_by_field_name("superclasses")
            {
                for i in 0..superclasses.named_child_count() {
                    if let Some(arg) = superclasses.named_child(i as u32) {
                        // Skip keyword_argument (e.g., metaclass=ABCMeta).
                        if arg.kind() == "keyword_argument" {
                            continue;
                        }
                        let parent = node_text(arg, src);
                        if !parent.is_empty() {
                            edges.push(RawTypeEdge {
                                child_name: class_name.to_string(),
                                parent_name: parent.to_string(),
                                relationship: "extends".to_string(),
                            });
                        }
                    }
                }
            }
        }
        Lang::Java => {
            if (kind == "class_declaration" || kind == "interface_declaration")
                && let Some(class_name) = field_text(node, "name", src)
            {
                let is_interface = kind == "interface_declaration";

                // extends: superclass field for classes.
                if !is_interface && let Some(superclass) = node.child_by_field_name("superclass") {
                    extract_java_type_list(superclass, src, class_name, "extends", edges);
                }

                // implements for classes, extends for interfaces.
                if let Some(interfaces) = node.child_by_field_name("interfaces") {
                    let rel = if is_interface {
                        "extends"
                    } else {
                        "implements"
                    };
                    extract_java_type_list(interfaces, src, class_name, rel, edges);
                }
            }
        }
        Lang::CSharp => {
            if matches!(
                kind,
                "class_declaration" | "struct_declaration" | "interface_declaration"
            ) && let Some(class_name) = field_text(node, "name", src)
            {
                let is_interface = kind == "interface_declaration";
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i as u32)
                        && child.kind() == "base_list"
                    {
                        extract_csharp_base_list(child, src, class_name, is_interface, edges);
                    }
                }
            }
        }
        Lang::Cpp => {
            if (kind == "class_specifier" || kind == "struct_specifier")
                && let Some(class_name) = field_text(node, "name", src)
            {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i as u32)
                        && child.kind() == "base_class_clause"
                    {
                        extract_cpp_bases(child, src, class_name, edges);
                    }
                }
            }
        }
        Lang::Ruby => {
            if kind == "class"
                && let Some(class_name) = field_text(node, "name", src)
                && let Some(superclass) = node.child_by_field_name("superclass")
            {
                for i in 0..superclass.child_count() {
                    if let Some(child) = superclass.child(i as u32)
                        && (child.kind() == "constant" || child.kind() == "scope_resolution")
                    {
                        let parent = node_text(child, src);
                        if !parent.is_empty() {
                            edges.push(RawTypeEdge {
                                child_name: class_name.to_string(),
                                parent_name: parent.to_string(),
                                relationship: "extends".to_string(),
                            });
                        }
                    }
                }
            }
        }
        Lang::Rust => {
            if kind == "impl_item"
                && let Some(trait_node) = node.child_by_field_name("trait")
                && let Some(type_node) = node.child_by_field_name("type")
            {
                let trait_name = extract_type_name(trait_node, src);
                let type_name = extract_type_name(type_node, src);
                if !trait_name.is_empty() && !type_name.is_empty() {
                    edges.push(RawTypeEdge {
                        child_name: type_name,
                        parent_name: trait_name,
                        relationship: "implements".to_string(),
                    });
                }
            }
        }
        Lang::Php => {
            if kind == "class_declaration"
                && let Some(class_name) = field_text(node, "name", src)
            {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i as u32) {
                        if child.kind() == "base_clause" {
                            extract_php_clause(child, src, class_name, "extends", edges);
                        } else if child.kind() == "class_interface_clause" {
                            extract_php_clause(child, src, class_name, "implements", edges);
                        }
                    }
                }
            }
        }
        Lang::C | Lang::Go => {} // handled by early return above
    }

    // Recurse into children.
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            walk_type_edges(child, src, lang, edges);
        }
    }
}

/// Extract the type name from a type node, handling generic_type by taking
/// just the type_identifier name.
fn extract_type_name(node: Node, src: &[u8]) -> String {
    if node.kind() == "generic_type" {
        // generic_type has a "name" field that is the type_identifier.
        if let Some(name) = field_text(node, "name", src) {
            return name.to_string();
        }
    }
    node_text(node, src).to_string()
}

/// Extract extends/implements from a TypeScript class_heritage node.
fn extract_ts_heritage(heritage: Node, src: &[u8], class_name: &str, edges: &mut Vec<RawTypeEdge>) {
    for i in 0..heritage.child_count() {
        let Some(clause) = heritage.child(i as u32) else {
            continue;
        };
        match clause.kind() {
            "extends_clause" => {
                if let Some(value) = clause.child_by_field_name("value") {
                    let parent = extract_type_name(value, src);
                    if !parent.is_empty() {
                        edges.push(RawTypeEdge {
                            child_name: class_name.to_string(),
                            parent_name: parent,
                            relationship: "extends".to_string(),
                        });
                    }
                }
            }
            "implements_clause" => {
                for j in 0..clause.child_count() {
                    if let Some(type_node) = clause.child(j as u32)
                        && matches!(type_node.kind(), "type_identifier" | "generic_type")
                    {
                        let parent = extract_type_name(type_node, src);
                        if !parent.is_empty() {
                            edges.push(RawTypeEdge {
                                child_name: class_name.to_string(),
                                parent_name: parent,
                                relationship: "implements".to_string(),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Extract type identifiers from a Java superclass or super_interfaces node.
fn extract_java_type_list(
    node: Node,
    src: &[u8],
    class_name: &str,
    relationship: &str,
    edges: &mut Vec<RawTypeEdge>,
) {
    for i in 0..node.child_count() {
        let Some(child) = node.child(i as u32) else {
            continue;
        };
        match child.kind() {
            "type_identifier" | "generic_type" => {
                let parent = extract_type_name(child, src);
                if !parent.is_empty() {
                    edges.push(RawTypeEdge {
                        child_name: class_name.to_string(),
                        parent_name: parent,
                        relationship: relationship.to_string(),
                    });
                }
            }
            "type_list" => {
                extract_java_type_list(child, src, class_name, relationship, edges);
            }
            _ => {}
        }
    }
}

/// Extract type identifiers from a C# base_list node.
///
/// For classes/structs: first identifier = extends, rest = implements.
/// For interfaces: all identifiers = extends.
fn extract_csharp_base_list(
    base_list: Node,
    src: &[u8],
    class_name: &str,
    is_interface: bool,
    edges: &mut Vec<RawTypeEdge>,
) {
    let mut found_first = false;
    for i in 0..base_list.child_count() {
        if let Some(child) = base_list.child(i as u32)
            && matches!(
                child.kind(),
                "identifier" | "generic_name" | "qualified_name"
            )
        {
            let parent = extract_type_name(child, src);
            if parent.is_empty() {
                continue;
            }

            let rel = if is_interface {
                "extends"
            } else if !found_first {
                found_first = true;
                "extends"
            } else {
                "implements"
            };

            edges.push(RawTypeEdge {
                child_name: class_name.to_string(),
                parent_name: parent,
                relationship: rel.to_string(),
            });
        }
    }
}

/// Extract base class identifiers from a C++ base_class_clause.
fn extract_cpp_bases(
    base_clause: Node,
    src: &[u8],
    class_name: &str,
    edges: &mut Vec<RawTypeEdge>,
) {
    for i in 0..base_clause.child_count() {
        if let Some(child) = base_clause.child(i as u32)
            && matches!(
                child.kind(),
                "type_identifier" | "qualified_identifier" | "template_type"
            )
        {
            let parent = extract_type_name(child, src);
            if !parent.is_empty() {
                edges.push(RawTypeEdge {
                    child_name: class_name.to_string(),
                    parent_name: parent,
                    relationship: "extends".to_string(),
                });
            }
        }
    }
}

/// Extract type identifiers from a PHP base_clause or class_interface_clause.
fn extract_php_clause(
    clause: Node,
    src: &[u8],
    class_name: &str,
    relationship: &str,
    edges: &mut Vec<RawTypeEdge>,
) {
    for i in 0..clause.child_count() {
        if let Some(child) = clause.child(i as u32)
            && matches!(child.kind(), "name" | "qualified_name")
        {
            let parent = node_text(child, src);
            if !parent.is_empty() {
                edges.push(RawTypeEdge {
                    child_name: class_name.to_string(),
                    parent_name: parent.to_string(),
                    relationship: relationship.to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---------- detect_language tests ----------

    #[test]
    fn detect_typescript() {
        assert_eq!(detect_language(Path::new("a.ts")), Some(Lang::TypeScript));
    }

    #[test]
    fn detect_tsx() {
        assert_eq!(detect_language(Path::new("a.tsx")), Some(Lang::Tsx));
    }

    #[test]
    fn detect_javascript_js() {
        assert_eq!(detect_language(Path::new("a.js")), Some(Lang::JavaScript));
    }

    #[test]
    fn detect_javascript_jsx() {
        assert_eq!(detect_language(Path::new("a.jsx")), Some(Lang::JavaScript));
    }

    #[test]
    fn detect_python() {
        assert_eq!(detect_language(Path::new("a.py")), Some(Lang::Python));
    }

    #[test]
    fn detect_rust() {
        assert_eq!(detect_language(Path::new("a.rs")), Some(Lang::Rust));
    }

    #[test]
    fn detect_go() {
        assert_eq!(detect_language(Path::new("a.go")), Some(Lang::Go));
    }

    #[test]
    fn detect_java() {
        assert_eq!(detect_language(Path::new("a.java")), Some(Lang::Java));
    }

    #[test]
    fn detect_c() {
        assert_eq!(detect_language(Path::new("a.c")), Some(Lang::C));
    }

    #[test]
    fn detect_c_header() {
        assert_eq!(detect_language(Path::new("a.h")), Some(Lang::C));
    }

    #[test]
    fn detect_cpp_extensions() {
        for ext in &["cpp", "cc", "cxx", "hpp", "hh", "hxx"] {
            let p = PathBuf::from(format!("a.{ext}"));
            assert_eq!(detect_language(&p), Some(Lang::Cpp), "failed for .{ext}");
        }
    }

    #[test]
    fn detect_ruby() {
        assert_eq!(detect_language(Path::new("a.rb")), Some(Lang::Ruby));
    }

    #[test]
    fn detect_php() {
        assert_eq!(detect_language(Path::new("a.php")), Some(Lang::Php));
    }

    #[test]
    fn detect_unsupported_returns_none() {
        assert_eq!(detect_language(Path::new("a.txt")), None);
        assert_eq!(detect_language(Path::new("a.md")), None);
        assert_eq!(detect_language(Path::new("Makefile")), None);
    }

    #[test]
    fn detect_no_extension_returns_none() {
        assert_eq!(detect_language(Path::new("Dockerfile")), None);
    }

    // ---------- get_parser / grammar loading tests ----------

    #[test]
    fn parser_loads_all_grammars() {
        let langs = [
            Lang::TypeScript,
            Lang::Tsx,
            Lang::JavaScript,
            Lang::Python,
            Lang::Rust,
            Lang::Go,
            Lang::Java,
            Lang::C,
            Lang::Cpp,
            Lang::Ruby,
            Lang::Php,
            Lang::CSharp,
        ];
        for lang in langs {
            let _parser = get_parser(lang); // should not panic
        }
    }

    // ---------- parse_file tests ----------

    /// Helper: write `source` to a temp file with the given extension and parse it.
    fn parse_temp(ext: &str, source: &str) -> Option<(Tree, Lang)> {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join(format!("test.{ext}"));
        std::fs::write(&file_path, source).unwrap();
        parse_file(&file_path)
    }

    #[test]
    fn parse_rust_file() {
        let (tree, lang) = parse_temp("rs", "fn main() {}").unwrap();
        assert_eq!(lang, Lang::Rust);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_typescript_file() {
        let src = "function greet(name: string): void { console.log(name); }";
        let (tree, lang) = parse_temp("ts", src).unwrap();
        assert_eq!(lang, Lang::TypeScript);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_tsx_file() {
        let src = "const App = () => <div>hello</div>;";
        let (tree, lang) = parse_temp("tsx", src).unwrap();
        assert_eq!(lang, Lang::Tsx);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_javascript_file() {
        let src = "function add(a, b) { return a + b; }";
        let (tree, lang) = parse_temp("js", src).unwrap();
        assert_eq!(lang, Lang::JavaScript);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_python_file() {
        let src = "def hello():\n    pass\n";
        let (tree, lang) = parse_temp("py", src).unwrap();
        assert_eq!(lang, Lang::Python);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_go_file() {
        let src = "package main\n\nfunc main() {}\n";
        let (tree, lang) = parse_temp("go", src).unwrap();
        assert_eq!(lang, Lang::Go);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_java_file() {
        let src = "class Hello { public static void main(String[] args) {} }";
        let (tree, lang) = parse_temp("java", src).unwrap();
        assert_eq!(lang, Lang::Java);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_c_file() {
        let src = "int main() { return 0; }";
        let (tree, lang) = parse_temp("c", src).unwrap();
        assert_eq!(lang, Lang::C);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_cpp_file() {
        let src = "#include <iostream>\nint main() { return 0; }";
        let (tree, lang) = parse_temp("cpp", src).unwrap();
        assert_eq!(lang, Lang::Cpp);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_ruby_file() {
        let src = "def hello\n  puts 'hi'\nend\n";
        let (tree, lang) = parse_temp("rb", src).unwrap();
        assert_eq!(lang, Lang::Ruby);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_php_file() {
        let src = "<?php\nfunction hello() { echo 'hi'; }\n?>";
        let (tree, lang) = parse_temp("php", src).unwrap();
        assert_eq!(lang, Lang::Php);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_unsupported_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readme.txt");
        std::fs::write(&path, "hello world").unwrap();
        assert!(parse_file(&path).is_none());
    }

    #[test]
    fn parse_missing_file_returns_none() {
        assert!(parse_file(Path::new("/nonexistent/file.rs")).is_none());
    }

    // ---------- symbol extraction helper ----------

    /// Parse source code for a given language and extract symbols.
    fn extract_from(lang: Lang, source: &str) -> Vec<Symbol> {
        let mut parser = get_parser(lang);
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        extract_symbols(&tree, source, "test_file", lang)
    }

    /// Find a symbol by name in a list.
    fn find_sym<'a>(syms: &'a [Symbol], name: &str) -> &'a Symbol {
        syms.iter().find(|s| s.name == name).unwrap_or_else(|| {
            panic!(
                "symbol '{name}' not found in: {:?}",
                syms.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        })
    }

    // ---------- Rust symbol extraction ----------

    #[test]
    fn rust_function() {
        let syms = extract_from(Lang::Rust, "fn hello(x: i32) -> bool { true }");
        let s = find_sym(&syms, "hello");
        assert_eq!(s.kind, SymbolKind::Function);
        assert_eq!(s.line, 1);
        assert_eq!(s.language, "Rust");
        assert!(s.signature.contains("fn hello"));
    }

    #[test]
    fn rust_struct_and_enum() {
        let src = "struct Foo { x: i32 }\nenum Bar { A, B }";
        let syms = extract_from(Lang::Rust, src);
        let foo = find_sym(&syms, "Foo");
        assert_eq!(foo.kind, SymbolKind::Struct);
        let bar = find_sym(&syms, "Bar");
        assert_eq!(bar.kind, SymbolKind::Enum);
    }

    #[test]
    fn rust_trait() {
        let src = "trait MyTrait { fn do_thing(&self); }";
        let syms = extract_from(Lang::Rust, src);
        let t = find_sym(&syms, "MyTrait");
        assert_eq!(t.kind, SymbolKind::Trait);
        // The method inside the trait should be scoped
        let m = find_sym(&syms, "do_thing");
        assert_eq!(m.kind, SymbolKind::Method);
        assert_eq!(m.scope.as_deref(), Some("MyTrait"));
    }

    #[test]
    fn rust_impl_methods() {
        let src = "struct Point { x: f64 }\nimpl Point {\n    fn new() -> Self { todo!() }\n    fn distance(&self) -> f64 { 0.0 }\n}";
        let syms = extract_from(Lang::Rust, src);
        let new_fn = find_sym(&syms, "new");
        assert_eq!(new_fn.kind, SymbolKind::Method);
        assert_eq!(new_fn.scope.as_deref(), Some("Point"));
        let dist = find_sym(&syms, "distance");
        assert_eq!(dist.kind, SymbolKind::Method);
        assert_eq!(dist.scope.as_deref(), Some("Point"));
    }

    #[test]
    fn rust_const_static_type_mod() {
        let src = "const MAX: usize = 100;\nstatic GLOBAL: i32 = 42;\ntype Result<T> = std::result::Result<T, MyError>;\nmod inner {}";
        let syms = extract_from(Lang::Rust, src);
        let c = find_sym(&syms, "MAX");
        assert_eq!(c.kind, SymbolKind::Constant);
        let s = find_sym(&syms, "GLOBAL");
        assert_eq!(s.kind, SymbolKind::Variable);
        let t = find_sym(&syms, "Result");
        assert_eq!(t.kind, SymbolKind::TypeAlias);
        let m = find_sym(&syms, "inner");
        assert_eq!(m.kind, SymbolKind::Module);
    }

    // ---------- Python symbol extraction ----------

    #[test]
    fn python_function_and_class() {
        let src = "def greet(name):\n    print(name)\n\nclass Animal:\n    def speak(self):\n        pass\n";
        let syms = extract_from(Lang::Python, src);
        let greet = find_sym(&syms, "greet");
        assert_eq!(greet.kind, SymbolKind::Function);
        assert!(greet.scope.is_none());
        let animal = find_sym(&syms, "Animal");
        assert_eq!(animal.kind, SymbolKind::Class);
        let speak = find_sym(&syms, "speak");
        assert_eq!(speak.kind, SymbolKind::Method);
        assert_eq!(speak.scope.as_deref(), Some("Animal"));
    }

    #[test]
    fn python_module_level_constants() {
        let src = "MAX_SIZE = 100\nDEBUG = True\nmy_var = 42\n";
        let syms = extract_from(Lang::Python, src);
        let max = find_sym(&syms, "MAX_SIZE");
        assert_eq!(max.kind, SymbolKind::Constant);
        let debug = find_sym(&syms, "DEBUG");
        assert_eq!(debug.kind, SymbolKind::Constant);
        let var = find_sym(&syms, "my_var");
        assert_eq!(var.kind, SymbolKind::Variable);
    }

    // ---------- JavaScript symbol extraction ----------

    #[test]
    fn js_function_declaration() {
        let src = "function add(a, b) { return a + b; }";
        let syms = extract_from(Lang::JavaScript, src);
        let f = find_sym(&syms, "add");
        assert_eq!(f.kind, SymbolKind::Function);
        assert_eq!(f.language, "JavaScript");
    }

    #[test]
    fn js_class_with_methods() {
        let src = "class Dog {\n  constructor(name) { this.name = name; }\n  bark() { return 'woof'; }\n}";
        let syms = extract_from(Lang::JavaScript, src);
        let dog = find_sym(&syms, "Dog");
        assert_eq!(dog.kind, SymbolKind::Class);
        let bark = find_sym(&syms, "bark");
        assert_eq!(bark.kind, SymbolKind::Method);
        assert_eq!(bark.scope.as_deref(), Some("Dog"));
    }

    #[test]
    fn js_arrow_function_const() {
        let src = "const greet = (name) => `hello ${name}`;";
        let syms = extract_from(Lang::JavaScript, src);
        let g = find_sym(&syms, "greet");
        assert_eq!(g.kind, SymbolKind::Function);
    }

    #[test]
    fn js_const_variable() {
        let src = "const MAX_SIZE = 100;\nlet count = 0;";
        let syms = extract_from(Lang::JavaScript, src);
        let max = find_sym(&syms, "MAX_SIZE");
        assert_eq!(max.kind, SymbolKind::Constant);
        let count = find_sym(&syms, "count");
        assert_eq!(count.kind, SymbolKind::Variable);
    }

    // ---------- TypeScript symbol extraction ----------

    #[test]
    fn ts_function_and_interface() {
        let src = "function greet(name: string): void {}\ninterface Greeter { greet(name: string): void; }";
        let syms = extract_from(Lang::TypeScript, src);
        let f = find_sym(&syms, "greet");
        assert_eq!(f.kind, SymbolKind::Function);
        assert_eq!(f.language, "TypeScript");
        let i = find_sym(&syms, "Greeter");
        assert_eq!(i.kind, SymbolKind::Interface);
    }

    #[test]
    fn ts_type_alias_and_enum() {
        let src = "type ID = string | number;\nenum Color { Red, Green, Blue }";
        let syms = extract_from(Lang::TypeScript, src);
        let ta = find_sym(&syms, "ID");
        assert_eq!(ta.kind, SymbolKind::TypeAlias);
        let e = find_sym(&syms, "Color");
        assert_eq!(e.kind, SymbolKind::Enum);
    }

    #[test]
    fn ts_class_with_methods() {
        let src = "class MyService {\n  private name: string;\n  constructor(name: string) { this.name = name; }\n  getName(): string { return this.name; }\n}";
        let syms = extract_from(Lang::TypeScript, src);
        let cls = find_sym(&syms, "MyService");
        assert_eq!(cls.kind, SymbolKind::Class);
        let method = find_sym(&syms, "getName");
        assert_eq!(method.kind, SymbolKind::Method);
        assert_eq!(method.scope.as_deref(), Some("MyService"));
    }

    #[test]
    fn ts_interface_members_scoped() {
        let src = "interface FastifyReply {\n  send(data: any): void;\n  status: number;\n}";
        let syms = extract_from(Lang::TypeScript, src);
        let iface = find_sym(&syms, "FastifyReply");
        assert_eq!(iface.kind, SymbolKind::Interface);
        let method = find_sym(&syms, "send");
        assert_eq!(method.kind, SymbolKind::Method);
        assert_eq!(method.scope.as_deref(), Some("FastifyReply"));
        let prop = find_sym(&syms, "status");
        assert_eq!(prop.kind, SymbolKind::Variable);
        assert_eq!(prop.scope.as_deref(), Some("FastifyReply"));
    }

    // ---------- TSX symbol extraction ----------

    #[test]
    fn tsx_arrow_component() {
        let src = "const App = () => { return <div>hello</div>; };";
        let syms = extract_from(Lang::Tsx, src);
        let app = find_sym(&syms, "App");
        assert_eq!(app.kind, SymbolKind::Function);
        assert_eq!(app.language, "TSX");
    }

    // ---------- Go symbol extraction ----------

    #[test]
    fn go_function_and_struct() {
        let src = "package main\n\nfunc main() {}\n\ntype Config struct {\n\tHost string\n}\n";
        let syms = extract_from(Lang::Go, src);
        let f = find_sym(&syms, "main");
        assert_eq!(f.kind, SymbolKind::Function);
        assert_eq!(f.language, "Go");
        let s = find_sym(&syms, "Config");
        assert_eq!(s.kind, SymbolKind::Struct);
    }

    #[test]
    fn go_method_with_receiver() {
        let src = "package main\n\ntype Server struct{}\n\nfunc (s *Server) Start() error { return nil }\n";
        let syms = extract_from(Lang::Go, src);
        let m = find_sym(&syms, "Start");
        assert_eq!(m.kind, SymbolKind::Method);
        assert_eq!(m.scope.as_deref(), Some("Server"));
    }

    #[test]
    fn go_interface() {
        let src = "package main\n\ntype Reader interface {\n\tRead(p []byte) (int, error)\n}\n";
        let syms = extract_from(Lang::Go, src);
        let i = find_sym(&syms, "Reader");
        assert_eq!(i.kind, SymbolKind::Interface);
    }

    #[test]
    fn go_const_and_var() {
        let src = "package main\n\nconst MaxSize = 100\nvar Debug = false\n";
        let syms = extract_from(Lang::Go, src);
        let c = find_sym(&syms, "MaxSize");
        assert_eq!(c.kind, SymbolKind::Constant);
        let v = find_sym(&syms, "Debug");
        assert_eq!(v.kind, SymbolKind::Variable);
    }

    // ---------- Java symbol extraction ----------

    #[test]
    fn java_class_and_methods() {
        let src = "public class Calculator {\n    public int add(int a, int b) { return a + b; }\n    public Calculator() {}\n}";
        let syms = extract_from(Lang::Java, src);
        let cls = find_sym(&syms, "Calculator");
        assert_eq!(cls.kind, SymbolKind::Class);
        let m = find_sym(&syms, "add");
        assert_eq!(m.kind, SymbolKind::Method);
        assert_eq!(m.scope.as_deref(), Some("Calculator"));
    }

    #[test]
    fn java_interface() {
        let src = "interface Comparable {\n    int compareTo(Object o);\n}";
        let syms = extract_from(Lang::Java, src);
        let i = find_sym(&syms, "Comparable");
        assert_eq!(i.kind, SymbolKind::Interface);
    }

    #[test]
    fn java_enum() {
        let src = "enum Direction { NORTH, SOUTH, EAST, WEST }";
        let syms = extract_from(Lang::Java, src);
        let e = find_sym(&syms, "Direction");
        assert_eq!(e.kind, SymbolKind::Enum);
    }

    #[test]
    fn java_final_field() {
        let src = "class Config {\n    static final int MAX = 100;\n    int count;\n}";
        let syms = extract_from(Lang::Java, src);
        let c = find_sym(&syms, "MAX");
        assert_eq!(c.kind, SymbolKind::Constant);
        assert_eq!(c.scope.as_deref(), Some("Config"));
        let v = find_sym(&syms, "count");
        assert_eq!(v.kind, SymbolKind::Variable);
    }

    // ---------- C symbol extraction ----------

    #[test]
    fn c_function() {
        let src = "int main(int argc, char **argv) { return 0; }";
        let syms = extract_from(Lang::C, src);
        let f = find_sym(&syms, "main");
        assert_eq!(f.kind, SymbolKind::Function);
        assert_eq!(f.language, "C");
    }

    #[test]
    fn c_struct_and_enum() {
        let src = "struct Point { int x; int y; };\nenum Color { RED, GREEN, BLUE };";
        let syms = extract_from(Lang::C, src);
        let s = find_sym(&syms, "Point");
        assert_eq!(s.kind, SymbolKind::Struct);
        let e = find_sym(&syms, "Color");
        assert_eq!(e.kind, SymbolKind::Enum);
    }

    #[test]
    fn c_define_constant() {
        let src = "#define MAX_SIZE 1024\nint foo() { return 0; }";
        let syms = extract_from(Lang::C, src);
        let c = find_sym(&syms, "MAX_SIZE");
        assert_eq!(c.kind, SymbolKind::Constant);
    }

    // ---------- C++ symbol extraction ----------

    #[test]
    fn cpp_class_with_method() {
        let src = "class Dog {\npublic:\n    void bark() { }\n};";
        let syms = extract_from(Lang::Cpp, src);
        let cls = find_sym(&syms, "Dog");
        assert_eq!(cls.kind, SymbolKind::Class);
        let m = find_sym(&syms, "bark");
        assert_eq!(m.kind, SymbolKind::Method);
        assert_eq!(m.scope.as_deref(), Some("Dog"));
    }

    #[test]
    fn cpp_namespace() {
        let src = "namespace mylib {\n    void helper() {}\n}";
        let syms = extract_from(Lang::Cpp, src);
        let ns = find_sym(&syms, "mylib");
        assert_eq!(ns.kind, SymbolKind::Module);
        let f = find_sym(&syms, "helper");
        assert_eq!(f.kind, SymbolKind::Function);
        assert_eq!(f.scope.as_deref(), Some("mylib"));
    }

    #[test]
    fn cpp_struct_and_enum() {
        let src = "struct Vec3 { float x, y, z; };\nenum Season { SPRING, SUMMER };";
        let syms = extract_from(Lang::Cpp, src);
        let s = find_sym(&syms, "Vec3");
        assert_eq!(s.kind, SymbolKind::Struct);
        let e = find_sym(&syms, "Season");
        assert_eq!(e.kind, SymbolKind::Enum);
    }

    // ---------- Ruby symbol extraction ----------

    #[test]
    fn ruby_method_and_class() {
        let src = "def greet(name)\n  puts name\nend\n\nclass Animal\n  def speak\n    'hello'\n  end\nend\n";
        let syms = extract_from(Lang::Ruby, src);
        let f = find_sym(&syms, "greet");
        assert_eq!(f.kind, SymbolKind::Function);
        assert_eq!(f.language, "Ruby");
        let cls = find_sym(&syms, "Animal");
        assert_eq!(cls.kind, SymbolKind::Class);
        let m = find_sym(&syms, "speak");
        assert_eq!(m.kind, SymbolKind::Method);
        assert_eq!(m.scope.as_deref(), Some("Animal"));
    }

    #[test]
    fn ruby_module() {
        let src = "module Utils\n  def self.helper\n    true\n  end\nend\n";
        let syms = extract_from(Lang::Ruby, src);
        let m = find_sym(&syms, "Utils");
        assert_eq!(m.kind, SymbolKind::Module);
    }

    // ---------- PHP symbol extraction ----------

    #[test]
    fn php_function_and_class() {
        let src = "<?php\nfunction greet($name) { echo $name; }\n\nclass Dog {\n    public function bark() { return 'woof'; }\n}\n?>";
        let syms = extract_from(Lang::Php, src);
        let f = find_sym(&syms, "greet");
        assert_eq!(f.kind, SymbolKind::Function);
        assert_eq!(f.language, "PHP");
        let cls = find_sym(&syms, "Dog");
        assert_eq!(cls.kind, SymbolKind::Class);
        let m = find_sym(&syms, "bark");
        assert_eq!(m.kind, SymbolKind::Method);
        assert_eq!(m.scope.as_deref(), Some("Dog"));
    }

    #[test]
    fn php_interface_and_trait() {
        let src = "<?php\ninterface Printable {\n    public function print();\n}\n\ntrait Loggable {\n    public function log() {}\n}\n?>";
        let syms = extract_from(Lang::Php, src);
        let i = find_sym(&syms, "Printable");
        assert_eq!(i.kind, SymbolKind::Interface);
        let t = find_sym(&syms, "Loggable");
        assert_eq!(t.kind, SymbolKind::Trait);
    }

    // ---------- Cross-cutting: line/col/end_line/file ----------

    #[test]
    fn symbol_has_correct_position() {
        let src = "fn first() {}\n\nfn second() {}";
        let syms = extract_from(Lang::Rust, src);
        let first = find_sym(&syms, "first");
        assert_eq!(first.line, 1);
        assert_eq!(first.col, 0);
        assert_eq!(first.file, "test_file");
        let second = find_sym(&syms, "second");
        assert_eq!(second.line, 3);
    }

    #[test]
    fn symbol_end_line_present() {
        let src = "fn multi_line(\n    x: i32,\n    y: i32,\n) -> i32 {\n    x + y\n}";
        let syms = extract_from(Lang::Rust, src);
        let f = find_sym(&syms, "multi_line");
        assert!(f.end_line.is_some());
        assert!(f.end_line.unwrap() > f.line);
    }

    // ---------- Empty source ----------

    #[test]
    fn empty_source_yields_no_symbols() {
        let syms = extract_from(Lang::Rust, "");
        assert!(syms.is_empty());
    }

    // ======================================================================
    // Reference extraction tests
    // ======================================================================

    /// Parse source and extract references for a given language.
    fn refs_from(lang: Lang, source: &str) -> Vec<Reference> {
        let mut parser = get_parser(lang);
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        extract_references(&tree, source, "test_file", lang)
    }

    /// Parse source and extract imports for a given language.
    fn imports_from(lang: Lang, source: &str) -> FileImports {
        let mut parser = get_parser(lang);
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        extract_imports(&tree, source, "test_file", lang)
    }

    /// Find a reference by name in a list.
    fn find_ref<'a>(refs: &'a [Reference], name: &str) -> &'a Reference {
        refs.iter().find(|r| r.name == name).unwrap_or_else(|| {
            panic!(
                "reference '{name}' not found in: {:?}",
                refs.iter().map(|r| &r.name).collect::<Vec<_>>()
            )
        })
    }

    /// Check that at least one reference with the given name and kind exists.
    fn has_ref(refs: &[Reference], name: &str, kind: ReferenceKind) -> bool {
        refs.iter().any(|r| r.name == name && r.kind == kind)
    }

    // ---------- Rust reference extraction ----------

    #[test]
    fn rust_call_reference() {
        let src = "fn main() {\n    foo();\n    bar::baz();\n}";
        let refs = refs_from(Lang::Rust, src);
        assert!(has_ref(&refs, "foo", ReferenceKind::Call));
        assert!(has_ref(&refs, "baz", ReferenceKind::Call));
    }

    #[test]
    fn rust_type_reference() {
        let src = "fn process(x: MyType) -> Result<String, Error> { todo!() }";
        let refs = refs_from(Lang::Rust, src);
        assert!(has_ref(&refs, "MyType", ReferenceKind::Type));
    }

    #[test]
    fn rust_import_reference() {
        let src = "use std::collections::HashMap;\nuse crate::types::Symbol;";
        let refs = refs_from(Lang::Rust, src);
        let import_refs: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            import_refs.len() >= 2,
            "expected at least 2 import refs, got {}",
            import_refs.len()
        );
    }

    #[test]
    fn rust_reference_has_context() {
        let src = "fn main() {\n    foo(42);\n}";
        let refs = refs_from(Lang::Rust, src);
        let r = find_ref(&refs, "foo");
        assert_eq!(r.kind, ReferenceKind::Call);
        assert!(
            r.context.contains("foo(42)"),
            "context was: {:?}",
            r.context
        );
        assert_eq!(r.file, "test_file");
        assert!(r.line > 0);
    }

    // ---------- Python reference extraction ----------

    #[test]
    fn python_call_reference() {
        let src = "def main():\n    print('hello')\n    os.path.join('a', 'b')\n";
        let refs = refs_from(Lang::Python, src);
        assert!(has_ref(&refs, "print", ReferenceKind::Call));
        assert!(has_ref(&refs, "join", ReferenceKind::Call));
    }

    #[test]
    fn python_import_reference() {
        let src = "import os\nfrom pathlib import Path\n";
        let refs = refs_from(Lang::Python, src);
        let import_refs: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            import_refs.len() >= 2,
            "expected at least 2 import refs, got {}: {:?}",
            import_refs.len(),
            import_refs
        );
    }

    // ---------- JavaScript reference extraction ----------

    #[test]
    fn js_call_reference() {
        let src = "function main() {\n  console.log('hello');\n  fetch('/api');\n}";
        let refs = refs_from(Lang::JavaScript, src);
        assert!(has_ref(&refs, "log", ReferenceKind::Call));
        assert!(has_ref(&refs, "fetch", ReferenceKind::Call));
    }

    #[test]
    fn js_import_reference() {
        let src = "import { foo } from './foo';\nimport bar from 'bar';";
        let refs = refs_from(Lang::JavaScript, src);
        let import_refs: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            import_refs.len() >= 2,
            "expected at least 2 import refs, got {}: {:?}",
            import_refs.len(),
            import_refs
        );
    }

    // ---------- TypeScript reference extraction ----------

    #[test]
    fn ts_call_reference() {
        let src = "function main(): void {\n  greet('world');\n}";
        let refs = refs_from(Lang::TypeScript, src);
        assert!(has_ref(&refs, "greet", ReferenceKind::Call));
    }

    #[test]
    fn ts_type_reference() {
        let src = "function process(x: MyType): Result {\n  return x;\n}";
        let refs = refs_from(Lang::TypeScript, src);
        assert!(has_ref(&refs, "MyType", ReferenceKind::Type));
        assert!(has_ref(&refs, "Result", ReferenceKind::Type));
    }

    #[test]
    fn ts_import_reference() {
        let src = "import { Component } from 'react';\nimport axios from 'axios';";
        let refs = refs_from(Lang::TypeScript, src);
        let import_refs: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            import_refs.len() >= 2,
            "expected at least 2 import refs, got {}",
            import_refs.len()
        );
    }

    // ---------- TSX reference extraction ----------

    #[test]
    fn tsx_call_and_type_reference() {
        let src =
            "import React from 'react';\nconst App: FC = () => { useState(0); return <div/>; };";
        let refs = refs_from(Lang::Tsx, src);
        assert!(has_ref(&refs, "useState", ReferenceKind::Call));
        assert!(has_ref(&refs, "FC", ReferenceKind::Type));
    }

    // ---------- Go reference extraction ----------

    #[test]
    fn go_call_reference() {
        let src = "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"hello\")\n}\n";
        let refs = refs_from(Lang::Go, src);
        assert!(has_ref(&refs, "Println", ReferenceKind::Call));
    }

    #[test]
    fn go_type_reference() {
        let src = "package main\n\ntype Server struct{}\n\nfunc process(s Server) error {\n\treturn nil\n}\n";
        let refs = refs_from(Lang::Go, src);
        assert!(has_ref(&refs, "Server", ReferenceKind::Type));
    }

    #[test]
    fn go_import_reference() {
        let src = "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\nfunc main() {}\n";
        let refs = refs_from(Lang::Go, src);
        assert!(has_ref(&refs, "fmt", ReferenceKind::Import));
        assert!(has_ref(&refs, "os", ReferenceKind::Import));
    }

    // ---------- Java reference extraction ----------

    #[test]
    fn java_call_reference() {
        let src = "class App {\n    void run() {\n        System.out.println(\"hello\");\n    }\n}";
        let refs = refs_from(Lang::Java, src);
        assert!(has_ref(&refs, "System.out.println", ReferenceKind::Call));
    }

    #[test]
    fn java_type_reference() {
        let src = "class App {\n    String name;\n    List<Integer> items;\n}";
        let refs = refs_from(Lang::Java, src);
        assert!(has_ref(&refs, "String", ReferenceKind::Type));
    }

    #[test]
    fn java_import_reference() {
        let src = "import java.util.List;\nimport java.io.File;\nclass App {}";
        let refs = refs_from(Lang::Java, src);
        let import_refs: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            import_refs.len() >= 2,
            "expected at least 2 import refs, got {}",
            import_refs.len()
        );
    }

    // ---------- C reference extraction ----------

    #[test]
    fn c_call_reference() {
        let src = "#include <stdio.h>\nint main() {\n    printf(\"hello\");\n    return 0;\n}";
        let refs = refs_from(Lang::C, src);
        assert!(has_ref(&refs, "printf", ReferenceKind::Call));
    }

    #[test]
    fn c_include_reference() {
        let src = "#include <stdio.h>\n#include \"myheader.h\"\nint main() { return 0; }";
        let refs = refs_from(Lang::C, src);
        assert!(has_ref(&refs, "stdio.h", ReferenceKind::Import));
        assert!(has_ref(&refs, "myheader.h", ReferenceKind::Import));
    }

    // ---------- C++ reference extraction ----------

    #[test]
    fn cpp_call_reference() {
        let src = "#include <iostream>\nint main() {\n    std::cout << \"hello\";\n    foo();\n    return 0;\n}";
        let refs = refs_from(Lang::Cpp, src);
        assert!(has_ref(&refs, "foo", ReferenceKind::Call));
    }

    #[test]
    fn cpp_include_reference() {
        let src = "#include <iostream>\n#include <vector>\nint main() { return 0; }";
        let refs = refs_from(Lang::Cpp, src);
        assert!(has_ref(&refs, "iostream", ReferenceKind::Import));
        assert!(has_ref(&refs, "vector", ReferenceKind::Import));
    }

    // ---------- Ruby reference extraction ----------

    #[test]
    fn ruby_call_reference() {
        let src = "def main\n  puts 'hello'\n  arr.push(42)\nend\n";
        let refs = refs_from(Lang::Ruby, src);
        assert!(has_ref(&refs, "puts", ReferenceKind::Call));
    }

    #[test]
    fn ruby_require_reference() {
        let src = "require 'json'\nrequire_relative 'helper'\n";
        let refs = refs_from(Lang::Ruby, src);
        assert!(has_ref(&refs, "json", ReferenceKind::Import));
        assert!(has_ref(&refs, "helper", ReferenceKind::Import));
    }

    // ---------- PHP reference extraction ----------

    #[test]
    fn php_call_reference() {
        let src = "<?php\nfunction main() {\n    echo strlen('hello');\n}\n?>";
        let refs = refs_from(Lang::Php, src);
        assert!(has_ref(&refs, "strlen", ReferenceKind::Call));
    }

    #[test]
    fn php_type_reference() {
        let src = "<?php\nfunction process(MyType $x): Result {\n    return $x;\n}\n?>";
        let refs = refs_from(Lang::Php, src);
        assert!(has_ref(&refs, "MyType", ReferenceKind::Type));
        assert!(has_ref(&refs, "Result", ReferenceKind::Type));
    }

    // ======================================================================
    // Import/export extraction tests
    // ======================================================================

    #[test]
    fn rust_imports() {
        let src = "use std::collections::HashMap;\nuse crate::types::Symbol;\nfn main() {}";
        let fi = imports_from(Lang::Rust, src);
        assert_eq!(fi.file, "test_file");
        assert!(fi.imports.len() >= 2, "imports: {:?}", fi.imports);
        assert!(fi.imports.iter().any(|i| i.contains("HashMap")));
        assert!(fi.imports.iter().any(|i| i.contains("Symbol")));
    }

    #[test]
    fn rust_exports() {
        let src = "pub fn hello() {}\npub struct Foo {}\nfn private() {}";
        let fi = imports_from(Lang::Rust, src);
        assert!(
            fi.exports.contains(&"hello".to_string()),
            "exports: {:?}",
            fi.exports
        );
        assert!(
            fi.exports.contains(&"Foo".to_string()),
            "exports: {:?}",
            fi.exports
        );
        assert!(
            !fi.exports.contains(&"private".to_string()),
            "should not export private fn"
        );
    }

    #[test]
    fn python_imports() {
        let src = "import os\nfrom pathlib import Path\ndef main(): pass\n";
        let fi = imports_from(Lang::Python, src);
        assert!(
            fi.imports.iter().any(|i| i.contains("os")),
            "imports: {:?}",
            fi.imports
        );
        assert!(
            fi.imports.iter().any(|i| i.contains("pathlib")),
            "imports: {:?}",
            fi.imports
        );
    }

    #[test]
    fn js_imports_and_exports() {
        let src = "import { foo } from './foo';\nexport function bar() {}\nexport default function baz() {}";
        let fi = imports_from(Lang::JavaScript, src);
        assert!(
            fi.imports.iter().any(|i| i.contains("./foo")),
            "imports: {:?}",
            fi.imports
        );
        assert!(
            fi.exports.contains(&"bar".to_string()),
            "exports: {:?}",
            fi.exports
        );
    }

    #[test]
    fn ts_imports_and_exports() {
        let src = "import { Component } from 'react';\nexport interface Greeter { greet(): void; }";
        let fi = imports_from(Lang::TypeScript, src);
        assert!(
            fi.imports.iter().any(|i| i.contains("react")),
            "imports: {:?}",
            fi.imports
        );
    }

    #[test]
    fn go_imports() {
        let src = "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\nfunc main() {}\n";
        let fi = imports_from(Lang::Go, src);
        assert!(
            fi.imports.contains(&"fmt".to_string()),
            "imports: {:?}",
            fi.imports
        );
        assert!(
            fi.imports.contains(&"os".to_string()),
            "imports: {:?}",
            fi.imports
        );
    }

    #[test]
    fn go_exports() {
        let src = "package main\n\nfunc Exported() {}\nfunc private() {}\n";
        let fi = imports_from(Lang::Go, src);
        assert!(
            fi.exports.contains(&"Exported".to_string()),
            "exports: {:?}",
            fi.exports
        );
        assert!(!fi.exports.contains(&"private".to_string()));
    }

    #[test]
    fn java_imports() {
        let src = "import java.util.List;\nimport java.io.File;\nclass App {}";
        let fi = imports_from(Lang::Java, src);
        assert!(fi.imports.len() >= 2, "imports: {:?}", fi.imports);
    }

    #[test]
    fn c_includes() {
        let src = "#include <stdio.h>\n#include \"myheader.h\"\nint main() { return 0; }";
        let fi = imports_from(Lang::C, src);
        assert!(
            fi.imports.contains(&"stdio.h".to_string()),
            "imports: {:?}",
            fi.imports
        );
        assert!(
            fi.imports.contains(&"myheader.h".to_string()),
            "imports: {:?}",
            fi.imports
        );
    }

    #[test]
    fn cpp_includes() {
        let src = "#include <iostream>\n#include <vector>\nint main() { return 0; }";
        let fi = imports_from(Lang::Cpp, src);
        assert!(
            fi.imports.contains(&"iostream".to_string()),
            "imports: {:?}",
            fi.imports
        );
        assert!(
            fi.imports.contains(&"vector".to_string()),
            "imports: {:?}",
            fi.imports
        );
    }

    #[test]
    fn ruby_requires() {
        let src = "require 'json'\nrequire_relative 'helper'\ndef main; end\n";
        let fi = imports_from(Lang::Ruby, src);
        assert!(
            fi.imports.contains(&"json".to_string()),
            "imports: {:?}",
            fi.imports
        );
        assert!(
            fi.imports.contains(&"helper".to_string()),
            "imports: {:?}",
            fi.imports
        );
    }

    // ---------- Reference context line ----------

    #[test]
    fn reference_context_is_full_source_line() {
        let src = "fn main() {\n    let x = foo(42);\n}";
        let refs = refs_from(Lang::Rust, src);
        let r = find_ref(&refs, "foo");
        assert_eq!(r.context, "let x = foo(42);");
    }

    // ---------- Empty source yields no refs ----------

    #[test]
    fn empty_source_yields_no_references() {
        let refs = refs_from(Lang::Rust, "");
        assert!(refs.is_empty());
    }

    #[test]
    fn empty_source_yields_empty_imports() {
        let fi = imports_from(Lang::Rust, "");
        assert!(fi.imports.is_empty());
        assert!(fi.exports.is_empty());
    }

    // ---------- C# language detection ----------

    #[test]
    fn detect_csharp() {
        assert_eq!(detect_language(Path::new("a.cs")), Some(Lang::CSharp));
    }

    // ---------- C# parsing ----------

    #[test]
    fn parse_csharp_file() {
        let src = "class Hello { static void Main() {} }";
        let (tree, lang) = parse_temp("cs", src).unwrap();
        assert_eq!(lang, Lang::CSharp);
        assert!(!tree.root_node().has_error());
    }

    // ---------- C# symbol extraction ----------

    #[test]
    fn csharp_class_and_methods() {
        let src = "public class Calculator {\n    public int Add(int a, int b) { return a + b; }\n    public Calculator() {}\n}";
        let syms = extract_from(Lang::CSharp, src);
        let cls = find_sym(&syms, "Calculator");
        assert_eq!(cls.kind, SymbolKind::Class);
        assert_eq!(cls.language, "C#");
        let m = find_sym(&syms, "Add");
        assert_eq!(m.kind, SymbolKind::Method);
        assert_eq!(m.scope.as_deref(), Some("Calculator"));
    }

    #[test]
    fn csharp_struct() {
        let src = "public struct Point {\n    public int X;\n    public int Y;\n}";
        let syms = extract_from(Lang::CSharp, src);
        let s = find_sym(&syms, "Point");
        assert_eq!(s.kind, SymbolKind::Struct);
    }

    #[test]
    fn csharp_interface() {
        let src = "public interface IComparable {\n    int CompareTo(object o);\n}";
        let syms = extract_from(Lang::CSharp, src);
        let i = find_sym(&syms, "IComparable");
        assert_eq!(i.kind, SymbolKind::Interface);
    }

    #[test]
    fn csharp_enum() {
        let src = "enum Direction { North, South, East, West }";
        let syms = extract_from(Lang::CSharp, src);
        let e = find_sym(&syms, "Direction");
        assert_eq!(e.kind, SymbolKind::Enum);
    }

    #[test]
    fn csharp_namespace() {
        let src = "namespace MyApp {\n    class Foo {}\n}";
        let syms = extract_from(Lang::CSharp, src);
        let ns = find_sym(&syms, "MyApp");
        assert_eq!(ns.kind, SymbolKind::Module);
        let cls = find_sym(&syms, "Foo");
        assert_eq!(cls.kind, SymbolKind::Class);
        assert_eq!(cls.scope.as_deref(), Some("MyApp"));
    }

    #[test]
    fn csharp_delegate() {
        let src = "public delegate void EventHandler(object sender);";
        let syms = extract_from(Lang::CSharp, src);
        let d = find_sym(&syms, "EventHandler");
        assert_eq!(d.kind, SymbolKind::TypeAlias);
    }

    #[test]
    fn csharp_const_and_field() {
        let src = "class Config {\n    public const int MAX = 100;\n    private int _count;\n}";
        let syms = extract_from(Lang::CSharp, src);
        let c = find_sym(&syms, "MAX");
        assert_eq!(c.kind, SymbolKind::Constant);
        assert_eq!(c.scope.as_deref(), Some("Config"));
        let v = find_sym(&syms, "_count");
        assert_eq!(v.kind, SymbolKind::Variable);
    }

    #[test]
    fn csharp_property() {
        let src = "class User {\n    public string Name { get; set; }\n}";
        let syms = extract_from(Lang::CSharp, src);
        let p = find_sym(&syms, "Name");
        assert_eq!(p.kind, SymbolKind::Method);
        assert_eq!(p.scope.as_deref(), Some("User"));
    }

    // ---------- C# reference extraction ----------

    #[test]
    fn csharp_call_reference() {
        let src = "class Foo {\n    void Bar() {\n        Console.WriteLine(\"hi\");\n        DoStuff();\n    }\n}";
        let refs = refs_from(Lang::CSharp, src);
        assert!(has_ref(&refs, "WriteLine", ReferenceKind::Call));
        assert!(has_ref(&refs, "DoStuff", ReferenceKind::Call));
    }

    #[test]
    fn csharp_import_reference() {
        let src = "using System;\nusing System.Collections.Generic;\nclass Foo {}";
        let refs = refs_from(Lang::CSharp, src);
        assert!(
            refs.iter()
                .any(|r| r.kind == ReferenceKind::Import && r.name.contains("System"))
        );
    }

    // ---------- C# import extraction ----------

    #[test]
    fn csharp_imports() {
        let src = "using System;\nusing System.Linq;\nclass Foo {}";
        let fi = imports_from(Lang::CSharp, src);
        assert!(fi.imports.iter().any(|i| i == "System"));
        assert!(fi.imports.iter().any(|i| i == "System.Linq"));
    }

    // ======================================================================
    // Enclosing function detection tests
    // ======================================================================

    /// Helper: parse source, find first call reference, return its caller_name.
    fn caller_of_first_call(lang: Lang, source: &str) -> Option<String> {
        let refs = refs_from(lang, source);
        refs.into_iter()
            .find(|r| r.kind == ReferenceKind::Call)
            .and_then(|r| r.caller_name)
    }

    #[test]
    fn enclosing_rust() {
        let src = "fn outer() {\n    inner();\n}";
        assert_eq!(caller_of_first_call(Lang::Rust, src), Some("outer".into()));
    }

    #[test]
    fn enclosing_python() {
        let src = "def outer():\n    inner()\n";
        assert_eq!(
            caller_of_first_call(Lang::Python, src),
            Some("outer".into())
        );
    }

    #[test]
    fn enclosing_javascript() {
        let src = "function outer() {\n    inner();\n}";
        assert_eq!(
            caller_of_first_call(Lang::JavaScript, src),
            Some("outer".into())
        );
    }

    #[test]
    fn enclosing_typescript() {
        let src = "function outer() {\n    inner();\n}";
        assert_eq!(
            caller_of_first_call(Lang::TypeScript, src),
            Some("outer".into())
        );
    }

    #[test]
    fn enclosing_tsx() {
        let src = "function Component() {\n    helper();\n    return null;\n}";
        assert_eq!(
            caller_of_first_call(Lang::Tsx, src),
            Some("Component".into())
        );
    }

    #[test]
    fn enclosing_go() {
        let src = "package main\n\nfunc outer() {\n    inner()\n}\n";
        assert_eq!(caller_of_first_call(Lang::Go, src), Some("outer".into()));
    }

    #[test]
    fn enclosing_java() {
        let src = "class Foo {\n    void outer() {\n        inner();\n    }\n}";
        assert_eq!(caller_of_first_call(Lang::Java, src), Some("outer".into()));
    }

    #[test]
    fn enclosing_c() {
        let src = "void outer() {\n    inner();\n}";
        assert_eq!(caller_of_first_call(Lang::C, src), Some("outer".into()));
    }

    #[test]
    fn enclosing_cpp() {
        let src = "void outer() {\n    inner();\n}";
        assert_eq!(caller_of_first_call(Lang::Cpp, src), Some("outer".into()));
    }

    #[test]
    fn enclosing_ruby() {
        let src = "def outer\n  inner()\nend\n";
        assert_eq!(caller_of_first_call(Lang::Ruby, src), Some("outer".into()));
    }

    #[test]
    fn enclosing_php() {
        let src = "<?php\nfunction outer() {\n    inner();\n}\n?>";
        assert_eq!(caller_of_first_call(Lang::Php, src), Some("outer".into()));
    }

    #[test]
    fn enclosing_csharp() {
        let src = "class Foo {\n    void Outer() {\n        Inner();\n    }\n}";
        assert_eq!(
            caller_of_first_call(Lang::CSharp, src),
            Some("Outer".into())
        );
    }

    #[test]
    fn enclosing_file_scope_returns_none() {
        // Call at file scope (no enclosing function).
        let src = "foo();";
        assert_eq!(caller_of_first_call(Lang::JavaScript, src), None);
    }

    #[test]
    fn enclosing_nested_finds_innermost() {
        let src = "function outer() {\n    function inner() {\n        target();\n    }\n}";
        assert_eq!(
            caller_of_first_call(Lang::JavaScript, src),
            Some("inner".into())
        );
    }

    #[test]
    fn enclosing_js_arrow_function() {
        let src = "const handler = () => {\n    doWork();\n};";
        assert_eq!(
            caller_of_first_call(Lang::JavaScript, src),
            Some("handler".into())
        );
    }

    #[test]
    fn enclosing_type_ref_has_no_caller() {
        // Type references should NOT get caller_name (only call refs do).
        let src = "fn process(x: MyType) {}";
        let refs = refs_from(Lang::Rust, src);
        let type_ref = refs
            .iter()
            .find(|r| r.kind == ReferenceKind::Type)
            .expect("should find a type reference for MyType");
        assert!(type_ref.caller_name.is_none());
    }

    // ======================================================================
    // Debug tree helpers (ignored by default)
    // ======================================================================

    /// Debug helper: print the tree structure to understand node kinds.
    #[allow(dead_code)]
    fn dump_tree(node: Node, src: &str, indent: usize) {
        let text = node.utf8_text(src.as_bytes()).unwrap_or("");
        let short_text = if text.len() > 60 { &text[..60] } else { text };
        eprintln!(
            "{:indent$}{} [{}:{}] {:?}",
            "",
            node.kind(),
            node.start_position().row,
            node.start_position().column,
            short_text,
            indent = indent,
        );
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                dump_tree(child, src, indent + 2);
            }
        }
    }

    #[test]
    #[ignore]
    fn debug_rust_trait_tree() {
        let src = "trait MyTrait { fn do_thing(&self); }";
        let mut parser = get_parser(Lang::Rust);
        let tree = parser.parse(src.as_bytes(), None).unwrap();
        dump_tree(tree.root_node(), src, 0);
    }

    #[test]
    #[ignore]
    fn debug_cpp_class_tree() {
        let src = "class Dog {\npublic:\n    void bark() { }\n};";
        let mut parser = get_parser(Lang::Cpp);
        let tree = parser.parse(src.as_bytes(), None).unwrap();
        dump_tree(tree.root_node(), src, 0);
    }

    #[test]
    #[ignore]
    fn debug_cpp_namespace_tree() {
        let src = "namespace mylib {\n    void helper() {}\n}";
        let mut parser = get_parser(Lang::Cpp);
        let tree = parser.parse(src.as_bytes(), None).unwrap();
        dump_tree(tree.root_node(), src, 0);
    }

    // -- compute_confidence tests -----------------------------------------------

    #[test]
    fn confidence_import_kind_is_0_95() {
        let r = Reference {
            name: "HashMap".into(),
            kind: ReferenceKind::Import,
            file: "a.rs".into(),
            line: 1,
            col: 0,
            context: "use std::collections::HashMap;".into(),
            caller_name: None,
            confidence: 0.5,
        };
        let symbols: Vec<Symbol> = vec![];
        let imports: Vec<String> = vec![];
        let score = compute_confidence(&r, &symbols, &imports);
        assert!(
            (score - 0.95).abs() < 1e-9,
            "import references should have confidence 0.95, got {score}"
        );
    }

    #[test]
    fn confidence_name_in_imports_is_0_95() {
        let r = Reference {
            name: "HashMap".into(),
            kind: ReferenceKind::Call,
            file: "a.rs".into(),
            line: 10,
            col: 4,
            context: "let m = HashMap::new();".into(),
            caller_name: Some("main".into()),
            confidence: 0.5,
        };
        let symbols: Vec<Symbol> = vec![];
        let imports = vec!["HashMap".to_string(), "Vec".to_string()];
        let score = compute_confidence(&r, &symbols, &imports);
        assert!(
            (score - 0.95).abs() < 1e-9,
            "import-resolved references should have confidence 0.95, got {score}"
        );
    }

    #[test]
    fn confidence_same_file_definition_is_0_85() {
        let r = Reference {
            name: "helper".into(),
            kind: ReferenceKind::Call,
            file: "a.rs".into(),
            line: 10,
            col: 4,
            context: "helper();".into(),
            caller_name: Some("main".into()),
            confidence: 0.5,
        };
        let symbols = vec![Symbol {
            name: "helper".into(),
            kind: SymbolKind::Function,
            file: "a.rs".into(),
            line: 1,
            col: 0,
            end_line: Some(5),
            scope: None,
            signature: "fn helper()".into(),
            language: "Rust".into(),
            doc_comment: None,
        }];
        let imports: Vec<String> = vec![];
        let score = compute_confidence(&r, &symbols, &imports);
        assert!(
            (score - 0.85).abs() < 1e-9,
            "same-file definition should have confidence 0.85, got {score}"
        );
    }

    #[test]
    fn confidence_same_scope_is_0_80() {
        // Reference inside "MyClass" calling a method defined in "MyClass" scope
        // but not the same name as a top-level symbol.
        let r = Reference {
            name: "do_work".into(),
            kind: ReferenceKind::Call,
            file: "a.rs".into(),
            line: 20,
            col: 8,
            context: "self.do_work();".into(),
            caller_name: Some("run".into()),
            confidence: 0.5,
        };
        // "run" is in scope "MyClass", "do_work" is also in scope "MyClass".
        let symbols = vec![
            Symbol {
                name: "run".into(),
                kind: SymbolKind::Method,
                file: "a.rs".into(),
                line: 15,
                col: 4,
                end_line: Some(25),
                scope: Some("MyClass".into()),
                signature: "fn run(&self)".into(),
                language: "Rust".into(),
                doc_comment: None,
            },
            Symbol {
                name: "do_work".into(),
                kind: SymbolKind::Method,
                file: "a.rs".into(),
                line: 30,
                col: 4,
                end_line: Some(35),
                scope: Some("MyClass".into()),
                signature: "fn do_work(&self)".into(),
                language: "Rust".into(),
                doc_comment: None,
            },
        ];
        let imports: Vec<String> = vec![];
        let score = compute_confidence(&r, &symbols, &imports);
        // do_work is in the same file (0.85) AND same scope as caller "run" (0.80).
        // Same-file def should win (0.85) since it's higher.
        assert!(
            (score - 0.85).abs() < 1e-9,
            "same-file definition should win over same-scope, got {score}"
        );
    }

    #[test]
    fn confidence_cross_file_default_is_0_50() {
        let r = Reference {
            name: "external_func".into(),
            kind: ReferenceKind::Call,
            file: "a.rs".into(),
            line: 10,
            col: 4,
            context: "external_func();".into(),
            caller_name: Some("main".into()),
            confidence: 0.5,
        };
        // No matching symbol in same file, no matching import.
        let symbols = vec![Symbol {
            name: "unrelated".into(),
            kind: SymbolKind::Function,
            file: "a.rs".into(),
            line: 1,
            col: 0,
            end_line: Some(5),
            scope: None,
            signature: "fn unrelated()".into(),
            language: "Rust".into(),
            doc_comment: None,
        }];
        let imports: Vec<String> = vec![];
        let score = compute_confidence(&r, &symbols, &imports);
        assert!(
            (score - 0.50).abs() < 1e-9,
            "cross-file name match should have confidence 0.50, got {score}"
        );
    }

    #[test]
    fn confidence_scope_only_match_is_0_80() {
        // The caller is in scope "MyClass", the reference target is also in scope
        // "MyClass" but lives in a DIFFERENT file. This tests the scope-only path.
        let r = Reference {
            name: "helper".into(),
            kind: ReferenceKind::Call,
            file: "a.rs".into(),
            line: 20,
            col: 8,
            context: "self.helper();".into(),
            caller_name: Some("run".into()),
            confidence: 0.5,
        };
        let symbols = vec![
            Symbol {
                name: "run".into(),
                kind: SymbolKind::Method,
                file: "a.rs".into(),
                line: 15,
                col: 4,
                end_line: Some(25),
                scope: Some("MyClass".into()),
                signature: "fn run(&self)".into(),
                language: "Rust".into(),
                doc_comment: None,
            },
            // helper is in scope "MyClass" but different file
            Symbol {
                name: "helper".into(),
                kind: SymbolKind::Method,
                file: "b.rs".into(),
                line: 5,
                col: 4,
                end_line: Some(10),
                scope: Some("MyClass".into()),
                signature: "fn helper(&self)".into(),
                language: "Rust".into(),
                doc_comment: None,
            },
        ];
        let imports: Vec<String> = vec![];
        let score = compute_confidence(&r, &symbols, &imports);
        assert!(
            (score - 0.80).abs() < 1e-9,
            "same-scope reference should have confidence 0.80, got {score}"
        );
    }

    // ---------- type edge extraction helper ----------

    /// Parse source code for a given language and extract type edges.
    fn edges_from(lang: Lang, source: &str) -> Vec<crate::types::RawTypeEdge> {
        let mut parser = get_parser(lang);
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        extract_type_edges(&tree, source, "test_file", lang)
    }

    /// Find edges by child name.
    fn find_edges_for<'a>(
        edges: &'a [crate::types::RawTypeEdge],
        child: &str,
    ) -> Vec<&'a crate::types::RawTypeEdge> {
        edges.iter().filter(|e| e.child_name == child).collect()
    }

    // ---------- C and Go: no type edges ----------

    #[test]
    fn type_edges_c_produces_none() {
        let src = "struct Foo { int x; };";
        let edges = edges_from(Lang::C, src);
        assert!(edges.is_empty(), "C should produce no type edges");
    }

    #[test]
    fn type_edges_go_produces_none() {
        let src = "package main\n\ntype Foo struct { X int }";
        let edges = edges_from(Lang::Go, src);
        assert!(edges.is_empty(), "Go should produce no type edges");
    }

    // ---------- TypeScript type edges ----------

    #[test]
    fn ts_type_edges_extends() {
        let src = "class Animal {}\nclass Dog extends Animal {}";
        let edges = edges_from(Lang::TypeScript, src);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_name, "Dog");
        assert_eq!(edges[0].parent_name, "Animal");
        assert_eq!(edges[0].relationship, "extends");
    }

    #[test]
    fn ts_type_edges_implements() {
        let src = "interface Runnable {}\nclass Worker implements Runnable {}";
        let edges = edges_from(Lang::TypeScript, src);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_name, "Worker");
        assert_eq!(edges[0].parent_name, "Runnable");
        assert_eq!(edges[0].relationship, "implements");
    }

    #[test]
    fn ts_type_edges_extends_and_implements() {
        let src = "class Base {}\ninterface Iface {}\nclass Child extends Base implements Iface {}";
        let edges = edges_from(Lang::TypeScript, src);
        assert_eq!(edges.len(), 2);
        let extends: Vec<_> = edges
            .iter()
            .filter(|e| e.relationship == "extends")
            .collect();
        let implements: Vec<_> = edges
            .iter()
            .filter(|e| e.relationship == "implements")
            .collect();
        assert_eq!(extends.len(), 1);
        assert_eq!(extends[0].child_name, "Child");
        assert_eq!(extends[0].parent_name, "Base");
        assert_eq!(implements.len(), 1);
        assert_eq!(implements[0].child_name, "Child");
        assert_eq!(implements[0].parent_name, "Iface");
    }

    // ---------- JavaScript type edges ----------

    #[test]
    fn js_type_edges_extends() {
        let src = "class Animal {}\nclass Dog extends Animal {}";
        let edges = edges_from(Lang::JavaScript, src);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_name, "Dog");
        assert_eq!(edges[0].parent_name, "Animal");
        assert_eq!(edges[0].relationship, "extends");
    }

    // ---------- Python type edges ----------

    #[test]
    fn python_type_edges_single_base() {
        let src = "class Animal:\n    pass\n\nclass Dog(Animal):\n    pass\n";
        let edges = edges_from(Lang::Python, src);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_name, "Dog");
        assert_eq!(edges[0].parent_name, "Animal");
        assert_eq!(edges[0].relationship, "extends");
    }

    #[test]
    fn python_type_edges_multiple_bases() {
        let src = "class A:\n    pass\nclass B:\n    pass\nclass C(A, B):\n    pass\n";
        let edges = edges_from(Lang::Python, src);
        let c_edges = find_edges_for(&edges, "C");
        assert_eq!(c_edges.len(), 2);
        assert!(c_edges.iter().any(|e| e.parent_name == "A"));
        assert!(c_edges.iter().any(|e| e.parent_name == "B"));
        assert!(c_edges.iter().all(|e| e.relationship == "extends"));
    }

    // ---------- Java type edges ----------

    #[test]
    fn java_type_edges_extends() {
        let src = "class Animal {}\nclass Dog extends Animal {}";
        let edges = edges_from(Lang::Java, src);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_name, "Dog");
        assert_eq!(edges[0].parent_name, "Animal");
        assert_eq!(edges[0].relationship, "extends");
    }

    #[test]
    fn java_type_edges_implements() {
        let src = "interface Runnable {}\nclass Worker implements Runnable {}";
        let edges = edges_from(Lang::Java, src);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_name, "Worker");
        assert_eq!(edges[0].parent_name, "Runnable");
        assert_eq!(edges[0].relationship, "implements");
    }

    // ---------- C# type edges ----------

    #[test]
    fn csharp_type_edges_extends() {
        let src = "class Animal {}\nclass Dog : Animal {}";
        let edges = edges_from(Lang::CSharp, src);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_name, "Dog");
        assert_eq!(edges[0].parent_name, "Animal");
        assert_eq!(edges[0].relationship, "extends");
    }

    #[test]
    fn csharp_type_edges_implements() {
        let src = "class Animal {}\ninterface IRunnable {}\nclass Dog : Animal, IRunnable {}";
        let edges = edges_from(Lang::CSharp, src);
        let dog_edges = find_edges_for(&edges, "Dog");
        assert_eq!(dog_edges.len(), 2);
        let extends: Vec<_> = dog_edges
            .iter()
            .filter(|e| e.relationship == "extends")
            .collect();
        let implements: Vec<_> = dog_edges
            .iter()
            .filter(|e| e.relationship == "implements")
            .collect();
        assert_eq!(extends.len(), 1);
        assert_eq!(extends[0].parent_name, "Animal");
        assert_eq!(implements.len(), 1);
        assert_eq!(implements[0].parent_name, "IRunnable");
    }

    // ---------- C++ type edges ----------

    #[test]
    fn cpp_type_edges_extends() {
        let src = "class Animal {};\nclass Dog : public Animal {};";
        let edges = edges_from(Lang::Cpp, src);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_name, "Dog");
        assert_eq!(edges[0].parent_name, "Animal");
        assert_eq!(edges[0].relationship, "extends");
    }

    // ---------- Ruby type edges ----------

    #[test]
    fn ruby_type_edges_extends() {
        let src = "class Animal\nend\nclass Dog < Animal\nend\n";
        let edges = edges_from(Lang::Ruby, src);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_name, "Dog");
        assert_eq!(edges[0].parent_name, "Animal");
        assert_eq!(edges[0].relationship, "extends");
    }

    // ---------- Rust type edges ----------

    #[test]
    fn rust_type_edges_impl_trait() {
        let src = "trait Drawable { fn draw(&self); }\nstruct Circle {}\nimpl Drawable for Circle { fn draw(&self) {} }";
        let edges = edges_from(Lang::Rust, src);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_name, "Circle");
        assert_eq!(edges[0].parent_name, "Drawable");
        assert_eq!(edges[0].relationship, "implements");
    }

    #[test]
    fn rust_type_edges_impl_no_trait() {
        let src = "struct Point { x: f64 }\nimpl Point { fn new() -> Self { todo!() } }";
        let edges = edges_from(Lang::Rust, src);
        assert!(
            edges.is_empty(),
            "inherent impl should produce no type edges"
        );
    }

    // ---------- PHP type edges ----------

    #[test]
    fn php_type_edges_extends() {
        let src = "<?php\nclass Animal {}\nclass Dog extends Animal {}";
        let edges = edges_from(Lang::Php, src);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_name, "Dog");
        assert_eq!(edges[0].parent_name, "Animal");
        assert_eq!(edges[0].relationship, "extends");
    }

    #[test]
    fn php_type_edges_implements() {
        let src = "<?php\ninterface Runnable {}\nclass Worker implements Runnable {}";
        let edges = edges_from(Lang::Php, src);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child_name, "Worker");
        assert_eq!(edges[0].parent_name, "Runnable");
        assert_eq!(edges[0].relationship, "implements");
    }
}
