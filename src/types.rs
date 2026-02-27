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

impl SymbolKind {
    /// Returns `true` for container types that can hold child symbols
    /// (class, struct, enum, trait, interface).
    pub fn is_container(self) -> bool {
        matches!(
            self,
            SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Enum
                | SymbolKind::Trait
                | SymbolKind::Interface
        )
    }
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
    /// Name of the enclosing function/method for call-site references.
    /// `None` for file-scope calls, type refs, and import refs.
    pub caller_name: Option<String>,
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

/// A symbol with its full source body, returned by `wonk show`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowResult {
    /// The symbol name.
    pub name: String,
    /// What kind of symbol this is.
    pub kind: SymbolKind,
    /// Path of the source file (relative to repo root).
    pub file: String,
    /// 1-based line number where the symbol starts.
    pub line: usize,
    /// 1-based line number where the symbol ends (if applicable).
    pub end_line: Option<usize>,
    /// Full source body (lines between start and end) or signature fallback.
    pub source: String,
    /// Language name (e.g. "Rust", "Python").
    pub language: String,
}

/// A caller of a symbol, discovered via the call graph (caller_id references).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerResult {
    /// Name of the calling function/method.
    pub caller_name: String,
    /// What kind of symbol the caller is.
    pub caller_kind: SymbolKind,
    /// File containing the caller definition.
    pub file: String,
    /// 1-based line number of the caller definition.
    pub line: usize,
    /// Signature of the calling function.
    pub signature: String,
    /// BFS depth at which this caller was discovered (1 = direct).
    pub depth: usize,
    /// File containing the specific definition that was called (when multiple
    /// definitions exist). `None` when there is only one definition.
    pub target_file: Option<String>,
}

/// A callee of a symbol, discovered via the call graph (caller_id references).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalleeResult {
    /// Name of the called function/symbol.
    pub callee_name: String,
    /// File where the call site (reference) is located.
    pub file: String,
    /// 1-based line number of the call site.
    pub line: usize,
    /// Source context of the call site.
    pub context: String,
    /// BFS depth at which this callee was discovered (1 = direct).
    pub depth: usize,
    /// File of the parent function that makes this call.
    pub source_file: Option<String>,
}

/// A single hop in a call path between two symbols, returned by `wonk callpath`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallPathHop {
    /// The symbol name at this hop.
    pub symbol_name: String,
    /// What kind of symbol this is.
    pub symbol_kind: SymbolKind,
    /// Path of the source file containing the symbol.
    pub file: String,
    /// 1-based line number of the symbol definition.
    pub line: usize,
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

    #[test]
    fn show_result_creation() {
        let sr = ShowResult {
            name: "processPayment".into(),
            kind: SymbolKind::Function,
            file: "src/billing.ts".into(),
            line: 10,
            end_line: Some(25),
            source: "function processPayment() {\n  // ...\n}".into(),
            language: "TypeScript".into(),
        };
        assert_eq!(sr.name, "processPayment");
        assert_eq!(sr.kind, SymbolKind::Function);
        assert_eq!(sr.file, "src/billing.ts");
        assert_eq!(sr.line, 10);
        assert_eq!(sr.end_line, Some(25));
        assert!(sr.source.contains("processPayment"));
        assert_eq!(sr.language, "TypeScript");
    }

    #[test]
    fn show_result_equality_by_value() {
        let a = ShowResult {
            name: "foo".into(),
            kind: SymbolKind::Function,
            file: "a.rs".into(),
            line: 1,
            end_line: Some(5),
            source: "fn foo() {}".into(),
            language: "Rust".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn is_container_true_for_container_kinds() {
        assert!(SymbolKind::Class.is_container());
        assert!(SymbolKind::Struct.is_container());
        assert!(SymbolKind::Enum.is_container());
        assert!(SymbolKind::Trait.is_container());
        assert!(SymbolKind::Interface.is_container());
    }

    #[test]
    fn is_container_false_for_non_container_kinds() {
        assert!(!SymbolKind::Function.is_container());
        assert!(!SymbolKind::Method.is_container());
        assert!(!SymbolKind::TypeAlias.is_container());
        assert!(!SymbolKind::Constant.is_container());
        assert!(!SymbolKind::Variable.is_container());
        assert!(!SymbolKind::Module.is_container());
    }

    #[test]
    fn show_result_without_end_line() {
        let sr = ShowResult {
            name: "MAX_SIZE".into(),
            kind: SymbolKind::Constant,
            file: "src/config.rs".into(),
            line: 3,
            end_line: None,
            source: "const MAX_SIZE: usize = 1024;".into(),
            language: "Rust".into(),
        };
        assert!(sr.end_line.is_none());
        assert!(sr.source.contains("MAX_SIZE"));
    }

    #[test]
    fn caller_result_creation() {
        let cr = CallerResult {
            caller_name: "dispatch".into(),
            caller_kind: SymbolKind::Function,
            file: "src/router.rs".into(),
            line: 50,
            signature: "fn dispatch()".into(),
            depth: 1,
            target_file: Some("src/db.rs".into()),
        };
        assert_eq!(cr.caller_name, "dispatch");
        assert_eq!(cr.caller_kind, SymbolKind::Function);
        assert_eq!(cr.file, "src/router.rs");
        assert_eq!(cr.line, 50);
        assert_eq!(cr.signature, "fn dispatch()");
        assert_eq!(cr.depth, 1);
        assert_eq!(cr.target_file.as_deref(), Some("src/db.rs"));
    }

    #[test]
    fn caller_result_equality_by_value() {
        let a = CallerResult {
            caller_name: "foo".into(),
            caller_kind: SymbolKind::Function,
            file: "a.rs".into(),
            line: 1,
            signature: "fn foo()".into(),
            depth: 1,
            target_file: None,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn callee_result_creation() {
        let cr = CalleeResult {
            callee_name: "open_db".into(),
            file: "src/db.rs".into(),
            line: 10,
            context: "    let conn = open_db(&path);".into(),
            depth: 1,
            source_file: Some("src/router.rs".into()),
        };
        assert_eq!(cr.callee_name, "open_db");
        assert_eq!(cr.file, "src/db.rs");
        assert_eq!(cr.line, 10);
        assert!(cr.context.contains("open_db"));
        assert_eq!(cr.depth, 1);
        assert_eq!(cr.source_file.as_deref(), Some("src/router.rs"));
    }

    #[test]
    fn callee_result_equality_by_value() {
        let a = CalleeResult {
            callee_name: "bar".into(),
            file: "b.rs".into(),
            line: 5,
            context: "bar()".into(),
            depth: 2,
            source_file: None,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn callpath_hop_creation() {
        let hop = CallPathHop {
            symbol_name: "dispatch".into(),
            symbol_kind: SymbolKind::Function,
            file: "src/router.rs".into(),
            line: 50,
        };
        assert_eq!(hop.symbol_name, "dispatch");
        assert_eq!(hop.symbol_kind, SymbolKind::Function);
        assert_eq!(hop.file, "src/router.rs");
        assert_eq!(hop.line, 50);
    }

    #[test]
    fn callpath_hop_equality_by_value() {
        let a = CallPathHop {
            symbol_name: "foo".into(),
            symbol_kind: SymbolKind::Function,
            file: "a.rs".into(),
            line: 1,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
