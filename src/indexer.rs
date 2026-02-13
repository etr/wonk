//! File indexing and tree-sitter parsing.
//!
//! Provides multi-language parsing infrastructure: language detection by file
//! extension, parser construction with the correct grammar, and file parsing.
//! Also extracts symbol definitions (functions, classes, types, etc.) from
//! parsed syntax trees across all supported languages.

use std::path::Path;

use tree_sitter::{Language, Node, Parser, Tree};

use crate::types::{FileImports, Reference, ReferenceKind, Symbol, SymbolKind};

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
            matches!(kind, "class_declaration" | "class")
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
    if let Some(r) = match_call_ref(node, kind, src, file, lang, source_lines) {
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
    }
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
    }

    // Recurse into children
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            walk_imports(child, src, lang, imports, exports);
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
}
