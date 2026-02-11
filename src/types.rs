//! Shared types and data structures.

use std::fmt;

/// The kind of a symbol definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Interface,
    Enum,
    Trait,
    TypeAlias,
    Constant,
    Variable,
    Module,
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Interface => "interface",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::TypeAlias => "type_alias",
            SymbolKind::Constant => "constant",
            SymbolKind::Variable => "variable",
            SymbolKind::Module => "module",
        };
        write!(f, "{s}")
    }
}

/// A symbol definition extracted from a parsed syntax tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// The symbol name (e.g. function name, class name).
    pub name: String,
    /// What kind of symbol this is.
    pub kind: SymbolKind,
    /// Path of the source file.
    pub file: String,
    /// 1-based line number where the symbol starts.
    pub line: usize,
    /// 0-based column offset where the symbol starts.
    pub col: usize,
    /// 1-based line number where the symbol ends (if applicable).
    pub end_line: Option<usize>,
    /// Parent symbol name (e.g. class name for a method).
    pub scope: Option<String>,
    /// Full signature text for display (e.g. the function header).
    pub signature: String,
    /// Language name (e.g. "Rust", "Python").
    pub language: String,
}

/// The kind of a reference (usage site, not a definition).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReferenceKind {
    /// A function or method call.
    Call,
    /// A type annotation or type reference.
    Type,
    /// An import / use statement.
    Import,
}

impl fmt::Display for ReferenceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ReferenceKind::Call => "call",
            ReferenceKind::Type => "type",
            ReferenceKind::Import => "import",
        };
        write!(f, "{s}")
    }
}

/// A reference (usage site) extracted from a parsed syntax tree.
///
/// References include function/method calls, type annotations, and import
/// statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The referenced name (e.g. function name, type name, imported module).
    pub name: String,
    /// What kind of reference this is.
    pub kind: ReferenceKind,
    /// Path of the source file containing this reference.
    pub file: String,
    /// 1-based line number where the reference occurs.
    pub line: usize,
    /// 0-based column offset where the reference occurs.
    pub col: usize,
    /// Full source line for display context.
    pub context: String,
}

/// Import and export data for a single file.
///
/// Used to build file-level dependency graphs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileImports {
    /// Path of the source file.
    pub file: String,
    /// Module/file paths imported by this file.
    pub imports: Vec<String>,
    /// Symbols exported from this file (for JS/TS `export` statements).
    pub exports: Vec<String>,
}
