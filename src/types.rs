//! Shared types and data structures.

use std::fmt;
use std::str::FromStr;

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

impl FromStr for SymbolKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "function" => Ok(SymbolKind::Function),
            "method" => Ok(SymbolKind::Method),
            "class" => Ok(SymbolKind::Class),
            "struct" => Ok(SymbolKind::Struct),
            "interface" => Ok(SymbolKind::Interface),
            "enum" => Ok(SymbolKind::Enum),
            "trait" => Ok(SymbolKind::Trait),
            "type_alias" => Ok(SymbolKind::TypeAlias),
            "constant" => Ok(SymbolKind::Constant),
            "variable" => Ok(SymbolKind::Variable),
            "module" => Ok(SymbolKind::Module),
            other => Err(format!("unknown symbol kind: {other}")),
        }
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

/// A member of a K-Means cluster, representing a single symbol embedding.
///
/// Derives `PartialEq` but not `Eq` because `distance_to_centroid` is `f32`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterMember {
    /// Database row ID of the symbol.
    pub symbol_id: i64,
    /// The symbol name (empty until resolved from DB).
    pub symbol_name: String,
    /// What kind of symbol this is (defaults to Function until resolved).
    pub symbol_kind: SymbolKind,
    /// Path of the source file (empty until resolved from DB).
    pub file: String,
    /// 1-based line number (0 until resolved from DB).
    pub line: usize,
    /// Euclidean distance from this point to the cluster centroid.
    pub distance_to_centroid: f32,
}

/// A cluster of symbol embeddings produced by K-Means.
///
/// Derives `PartialEq` but not `Eq` because centroid and member distances are `f32`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    /// Cluster index (0-based).
    pub cluster_id: usize,
    /// The centroid vector of this cluster.
    pub centroid: Vec<f32>,
    /// All members of this cluster, sorted by ascending distance to centroid.
    pub members: Vec<ClusterMember>,
    /// The top N members closest to the centroid (subset of `members`).
    pub representative_symbols: Vec<ClusterMember>,
}

/// The kind of change detected for a symbol between the current file and the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeType {
    /// Symbol exists in current file but not in the index.
    Added,
    /// Symbol exists in both but has a different signature.
    Modified,
    /// Symbol exists in the index but not in the current file.
    Removed,
}

impl fmt::Display for ChangeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ChangeType::Added => "added",
            ChangeType::Modified => "modified",
            ChangeType::Removed => "removed",
        };
        write!(f, "{s}")
    }
}

/// A symbol that changed between the current file on disk and the indexed version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedSymbol {
    /// The symbol name.
    pub name: String,
    /// What kind of symbol this is.
    pub kind: SymbolKind,
    /// Path of the source file (relative to repo root).
    pub file: String,
    /// 1-based line number (current for Added/Modified, last indexed for Removed).
    pub line: usize,
    /// What kind of change was detected.
    pub change_type: ChangeType,
}

/// A result from semantic (embedding-based) similarity search.
///
/// Contains the matched symbol's metadata and its cosine similarity score
/// relative to the query vector.  Derives `PartialEq` but not `Eq` because
/// `similarity_score` is `f32`.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticResult {
    /// Database row ID of the matched symbol.
    pub symbol_id: i64,
    /// Path of the source file containing the symbol.
    pub file: String,
    /// 1-based line number where the symbol starts.
    pub line: usize,
    /// The symbol name.
    pub symbol_name: String,
    /// What kind of symbol this is.
    pub symbol_kind: SymbolKind,
    /// Cosine similarity score (higher = more similar).
    pub similarity_score: f32,
}

/// A lightweight reference to a symbol's identity and location.
///
/// Used in impact analysis results to identify the changed and impacted symbols
/// without carrying the full [`Symbol`] payload.
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolRef {
    /// The symbol name.
    pub name: String,
    /// What kind of symbol this is.
    pub kind: SymbolKind,
    /// Path of the source file (relative to repo root).
    pub file: String,
    /// 1-based line number.
    pub line: usize,
}

impl From<&ChangedSymbol> for SymbolRef {
    fn from(cs: &ChangedSymbol) -> Self {
        Self {
            name: cs.name.clone(),
            kind: cs.kind,
            file: cs.file.clone(),
            line: cs.line,
        }
    }
}

/// A single impact analysis result linking a changed symbol to a semantically
/// similar symbol that may be affected.
///
/// Derives `PartialEq` but not `Eq` because `similarity_score` is `f32`.
#[derive(Debug, Clone, PartialEq)]
pub struct ImpactResult {
    /// The symbol that changed.
    pub changed_symbol: SymbolRef,
    /// The symbol that may be impacted.
    pub impacted_symbol: SymbolRef,
    /// Cosine similarity score between the changed and impacted symbols.
    pub similarity_score: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_ref_creation() {
        let sr = SymbolRef {
            name: "verify_token".into(),
            kind: SymbolKind::Function,
            file: "src/auth/middleware.ts".into(),
            line: 15,
        };
        assert_eq!(sr.name, "verify_token");
        assert_eq!(sr.kind, SymbolKind::Function);
        assert_eq!(sr.file, "src/auth/middleware.ts");
        assert_eq!(sr.line, 15);
    }

    #[test]
    fn impact_result_creation() {
        let changed = SymbolRef {
            name: "verify_token".into(),
            kind: SymbolKind::Function,
            file: "src/auth/middleware.ts".into(),
            line: 15,
        };
        let impacted = SymbolRef {
            name: "validate_session".into(),
            kind: SymbolKind::Function,
            file: "src/auth/session.ts".into(),
            line: 8,
        };
        let result = ImpactResult {
            changed_symbol: changed.clone(),
            impacted_symbol: impacted.clone(),
            similarity_score: 0.89,
        };
        assert_eq!(result.changed_symbol, changed);
        assert_eq!(result.impacted_symbol, impacted);
        assert!((result.similarity_score - 0.89).abs() < 1e-6);
    }

    #[test]
    fn impact_result_equality_by_value() {
        let a = ImpactResult {
            changed_symbol: SymbolRef {
                name: "foo".into(),
                kind: SymbolKind::Function,
                file: "a.rs".into(),
                line: 1,
            },
            impacted_symbol: SymbolRef {
                name: "bar".into(),
                kind: SymbolKind::Method,
                file: "b.rs".into(),
                line: 10,
            },
            similarity_score: 0.75,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn symbol_ref_from_changed_symbol() {
        let cs = ChangedSymbol {
            name: "hello".into(),
            kind: SymbolKind::Function,
            file: "src/lib.rs".into(),
            line: 5,
            change_type: ChangeType::Modified,
        };
        let sr = SymbolRef::from(&cs);
        assert_eq!(sr.name, "hello");
        assert_eq!(sr.kind, SymbolKind::Function);
        assert_eq!(sr.file, "src/lib.rs");
        assert_eq!(sr.line, 5);
    }
}
