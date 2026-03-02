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
#[derive(Debug, Clone, PartialEq)]
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
    /// Confidence score for this reference (0.0 = lowest, 1.0 = highest).
    /// Import-resolved: 0.95, same-file definition: 0.85, same-scope: 0.80,
    /// cross-file name match: 0.50 (default).
    pub confidence: f64,
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
#[derive(Debug, Clone, PartialEq)]
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
    /// Confidence score of the underlying reference (0.0-1.0).
    pub confidence: f64,
}

/// A callee of a symbol, discovered via the call graph (caller_id references).
#[derive(Debug, Clone, PartialEq)]
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
    /// Confidence score of the underlying reference (0.0-1.0).
    pub confidence: f64,
}

// ---------------------------------------------------------------------------
// Summary types
// ---------------------------------------------------------------------------

/// Detail level for `wonk summary` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailLevel {
    /// All metrics: file count, line count, symbol counts, language breakdown, dependency count.
    Rich,
    /// Lightweight: file count, symbol count, languages only.
    Light,
    /// Symbol counts by kind only.
    Symbols,
}

impl fmt::Display for DetailLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DetailLevel::Rich => "rich",
            DetailLevel::Light => "light",
            DetailLevel::Symbols => "symbols",
        };
        write!(f, "{s}")
    }
}

impl FromStr for DetailLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "rich" => Ok(DetailLevel::Rich),
            "light" => Ok(DetailLevel::Light),
            "symbols" => Ok(DetailLevel::Symbols),
            other => Err(format!(
                "unknown detail level: {other} (expected: rich, light, symbols)"
            )),
        }
    }
}

/// Whether a summary path refers to a file or directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryPathType {
    File,
    Directory,
}

impl fmt::Display for SummaryPathType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SummaryPathType::File => "file",
            SummaryPathType::Directory => "directory",
        };
        write!(f, "{s}")
    }
}

/// Aggregated structural metrics for a file or directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryMetrics {
    /// Number of indexed files.
    pub file_count: usize,
    /// Total line count across all files.
    pub line_count: usize,
    /// Symbol counts by kind, sorted alphabetically.
    pub symbol_counts: Vec<(String, usize)>,
    /// File counts by language, sorted alphabetically.
    pub language_breakdown: Vec<(String, usize)>,
    /// Number of distinct import paths (dependencies).
    pub dependency_count: usize,
}

/// Structural summary result for a path (file or directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryResult {
    /// The path being summarized (relative to repo root).
    pub path: String,
    /// Whether this path is a file or directory.
    pub path_type: SummaryPathType,
    /// The detail level used.
    pub detail_level: DetailLevel,
    /// Aggregated metrics.
    pub metrics: SummaryMetrics,
    /// Child summaries (populated when depth > 0).
    pub children: Vec<SummaryResult>,
    /// Optional natural language description (populated by `--semantic`, TASK-064).
    pub description: Option<String>,
}

/// A raw (name-based) type hierarchy edge extracted from a syntax tree.
///
/// Contains unresolved names (not database IDs).  The pipeline resolves
/// `child_name` and `parent_name` to symbol IDs before inserting into the
/// `type_edges` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTypeEdge {
    /// Name of the child type (e.g. the class that extends or implements).
    pub child_name: String,
    /// Name of the parent type (e.g. the superclass or implemented interface).
    pub parent_name: String,
    /// Relationship kind: `"extends"` or `"implements"`.
    pub relationship: String,
}

/// A single step in an execution flow, representing a symbol at a given BFS depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowStep {
    /// The symbol name (e.g. function or method name).
    pub name: String,
    /// What kind of symbol this is.
    pub kind: SymbolKind,
    /// Path of the source file.
    pub file: String,
    /// 1-based line number where the symbol starts.
    pub line: usize,
    /// BFS depth at which this step was discovered (0 = entry point).
    pub depth: usize,
}

/// An execution flow traced from an entry point via BFS callee expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionFlow {
    /// The entry point of this flow.
    pub entry_point: FlowStep,
    /// All steps in the flow (including the entry point).
    pub steps: Vec<FlowStep>,
}

// ---------------------------------------------------------------------------
// Blast radius types
// ---------------------------------------------------------------------------

/// Direction of blast radius traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlastDirection {
    /// Traverse callers (who calls this symbol?).
    Upstream,
    /// Traverse callees (what does this symbol call?).
    Downstream,
}

impl fmt::Display for BlastDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BlastDirection::Upstream => "upstream",
            BlastDirection::Downstream => "downstream",
        };
        write!(f, "{s}")
    }
}

impl FromStr for BlastDirection {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "upstream" => Ok(BlastDirection::Upstream),
            "downstream" => Ok(BlastDirection::Downstream),
            other => Err(format!(
                "unknown blast direction: {other} (expected: upstream, downstream)"
            )),
        }
    }
}

/// Severity tier based on BFS depth from the target symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlastSeverity {
    /// Depth 1: direct dependants that will definitely break.
    WillBreak,
    /// Depth 2: symbols one hop removed, likely affected.
    LikelyAffected,
    /// Depth 3+: symbols further out that may need testing.
    MayNeedTesting,
}

impl fmt::Display for BlastSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BlastSeverity::WillBreak => "WILL BREAK",
            BlastSeverity::LikelyAffected => "LIKELY AFFECTED",
            BlastSeverity::MayNeedTesting => "MAY NEED TESTING",
        };
        write!(f, "{s}")
    }
}

/// Overall risk level based on total affected symbol count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BlastRiskLevel {
    /// 0-3 affected symbols.
    Low,
    /// 4-10 affected symbols.
    Medium,
    /// 11-25 affected symbols.
    High,
    /// More than 25 affected symbols.
    Critical,
}

impl fmt::Display for BlastRiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BlastRiskLevel::Low => "LOW",
            BlastRiskLevel::Medium => "MEDIUM",
            BlastRiskLevel::High => "HIGH",
            BlastRiskLevel::Critical => "CRITICAL",
        };
        write!(f, "{s}")
    }
}

/// A symbol affected by a blast radius analysis.
#[derive(Debug, Clone, PartialEq)]
pub struct BlastAffectedSymbol {
    /// The symbol name.
    pub name: String,
    /// What kind of symbol this is.
    pub kind: SymbolKind,
    /// Path of the source file.
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    /// BFS depth at which this symbol was discovered.
    pub depth: usize,
    /// Confidence score of the edge (0.0-1.0).
    pub confidence: f64,
}

/// A group of affected symbols at the same severity tier.
#[derive(Debug, Clone, PartialEq)]
pub struct BlastTier {
    /// The severity label for this group.
    pub severity: BlastSeverity,
    /// All affected symbols at this severity level.
    pub symbols: Vec<BlastAffectedSymbol>,
}

/// Complete blast radius analysis result.
#[derive(Debug, Clone, PartialEq)]
pub struct BlastAnalysis {
    /// The target symbol being analyzed.
    pub target: String,
    /// Direction of traversal.
    pub direction: BlastDirection,
    /// Overall risk level.
    pub risk_level: BlastRiskLevel,
    /// Total number of affected symbols across all tiers.
    pub total_affected: usize,
    /// Affected symbols grouped by severity tier.
    pub tiers: Vec<BlastTier>,
    /// Deduplicated list of files containing affected symbols.
    pub affected_files: Vec<String>,
}

// ---------------------------------------------------------------------------
// Change scope types (TASK-071)
// ---------------------------------------------------------------------------

/// Scope for change detection: which git diff to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeScope {
    /// Unstaged changes (working tree vs index): `git diff`.
    Unstaged,
    /// Staged changes (index vs HEAD): `git diff --cached`.
    Staged,
    /// All uncommitted changes (working tree vs HEAD): `git diff HEAD`.
    All,
    /// Compare against a specific git ref: `git diff <ref>`.
    Compare(String),
}

impl fmt::Display for ChangeScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChangeScope::Unstaged => write!(f, "unstaged"),
            ChangeScope::Staged => write!(f, "staged"),
            ChangeScope::All => write!(f, "all"),
            ChangeScope::Compare(r) => write!(f, "compare({r})"),
        }
    }
}

impl FromStr for ChangeScope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "unstaged" => Ok(ChangeScope::Unstaged),
            "staged" => Ok(ChangeScope::Staged),
            "all" => Ok(ChangeScope::All),
            other => Err(format!(
                "unknown scope: {other} (expected: unstaged, staged, all, compare)"
            )),
        }
    }
}

/// Result of scoped change analysis: all changed symbols across files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeAnalysis {
    /// The scope that was used for detection.
    pub scope: ChangeScope,
    /// All changed symbols found across the scoped files.
    pub changed_symbols: Vec<ChangedSymbol>,
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
            confidence: 0.85,
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
            confidence: 0.5,
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
            confidence: 0.85,
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
            confidence: 0.5,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn reference_confidence_field() {
        let r = Reference {
            name: "foo".into(),
            kind: ReferenceKind::Call,
            file: "a.rs".into(),
            line: 10,
            col: 4,
            context: "foo()".into(),
            caller_name: Some("bar".into()),
            confidence: 0.85,
        };
        assert!((r.confidence - 0.85).abs() < 1e-9);
    }

    #[test]
    fn reference_default_confidence() {
        let r = Reference {
            name: "baz".into(),
            kind: ReferenceKind::Import,
            file: "b.rs".into(),
            line: 1,
            col: 0,
            context: "use baz;".into(),
            caller_name: None,
            confidence: 0.5,
        };
        assert!((r.confidence - 0.5).abs() < 1e-9);
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

    // -- DetailLevel tests ---------------------------------------------------

    #[test]
    fn detail_level_display_round_trip() {
        for (level, expected) in [
            (DetailLevel::Rich, "rich"),
            (DetailLevel::Light, "light"),
            (DetailLevel::Symbols, "symbols"),
        ] {
            assert_eq!(level.to_string(), expected);
            assert_eq!(DetailLevel::from_str(expected).unwrap(), level);
        }
    }

    #[test]
    fn detail_level_from_str_invalid() {
        assert!(DetailLevel::from_str("unknown").is_err());
    }

    #[test]
    fn summary_path_type_display_round_trip() {
        for (pt, expected) in [
            (SummaryPathType::File, "file"),
            (SummaryPathType::Directory, "directory"),
        ] {
            assert_eq!(pt.to_string(), expected);
        }
    }

    #[test]
    fn summary_metrics_creation() {
        let m = SummaryMetrics {
            file_count: 10,
            line_count: 500,
            symbol_counts: vec![("function".into(), 20), ("class".into(), 5)],
            language_breakdown: vec![("Rust".into(), 8), ("Python".into(), 2)],
            dependency_count: 15,
        };
        assert_eq!(m.file_count, 10);
        assert_eq!(m.line_count, 500);
        assert_eq!(m.symbol_counts.len(), 2);
        assert_eq!(m.language_breakdown.len(), 2);
        assert_eq!(m.dependency_count, 15);
    }

    #[test]
    fn summary_result_creation() {
        let sr = SummaryResult {
            path: "src/".into(),
            path_type: SummaryPathType::Directory,
            detail_level: DetailLevel::Rich,
            metrics: SummaryMetrics {
                file_count: 5,
                line_count: 200,
                symbol_counts: vec![("function".into(), 10)],
                language_breakdown: vec![("Rust".into(), 5)],
                dependency_count: 3,
            },
            children: vec![],
            description: None,
        };
        assert_eq!(sr.path, "src/");
        assert_eq!(sr.path_type, SummaryPathType::Directory);
        assert_eq!(sr.detail_level, DetailLevel::Rich);
        assert_eq!(sr.metrics.file_count, 5);
        assert!(sr.children.is_empty());
        assert!(sr.description.is_none());
    }

    #[test]
    fn summary_result_with_children() {
        let child = SummaryResult {
            path: "src/lib.rs".into(),
            path_type: SummaryPathType::File,
            detail_level: DetailLevel::Rich,
            metrics: SummaryMetrics {
                file_count: 1,
                line_count: 100,
                symbol_counts: vec![("function".into(), 5)],
                language_breakdown: vec![("Rust".into(), 1)],
                dependency_count: 1,
            },
            children: vec![],
            description: None,
        };
        let parent = SummaryResult {
            path: "src/".into(),
            path_type: SummaryPathType::Directory,
            detail_level: DetailLevel::Rich,
            metrics: SummaryMetrics {
                file_count: 3,
                line_count: 300,
                symbol_counts: vec![],
                language_breakdown: vec![],
                dependency_count: 0,
            },
            children: vec![child],
            description: None,
        };
        assert_eq!(parent.children.len(), 1);
        assert_eq!(parent.children[0].path, "src/lib.rs");
    }

    #[test]
    fn raw_type_edge_creation() {
        let edge = RawTypeEdge {
            child_name: "Dog".into(),
            parent_name: "Animal".into(),
            relationship: "extends".into(),
        };
        assert_eq!(edge.child_name, "Dog");
        assert_eq!(edge.parent_name, "Animal");
        assert_eq!(edge.relationship, "extends");
    }

    #[test]
    fn raw_type_edge_equality() {
        let a = RawTypeEdge {
            child_name: "Cat".into(),
            parent_name: "Animal".into(),
            relationship: "extends".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // -- FlowStep tests -------------------------------------------------------

    #[test]
    fn flow_step_creation() {
        let step = FlowStep {
            name: "main".into(),
            kind: SymbolKind::Function,
            file: "src/main.rs".into(),
            line: 1,
            depth: 0,
        };
        assert_eq!(step.name, "main");
        assert_eq!(step.kind, SymbolKind::Function);
        assert_eq!(step.file, "src/main.rs");
        assert_eq!(step.line, 1);
        assert_eq!(step.depth, 0);
    }

    #[test]
    fn flow_step_equality_by_value() {
        let a = FlowStep {
            name: "dispatch".into(),
            kind: SymbolKind::Function,
            file: "src/router.rs".into(),
            line: 50,
            depth: 1,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // -- ExecutionFlow tests ---------------------------------------------------

    #[test]
    fn execution_flow_creation() {
        let entry = FlowStep {
            name: "main".into(),
            kind: SymbolKind::Function,
            file: "src/main.rs".into(),
            line: 1,
            depth: 0,
        };
        let steps = vec![
            entry.clone(),
            FlowStep {
                name: "dispatch".into(),
                kind: SymbolKind::Function,
                file: "src/router.rs".into(),
                line: 50,
                depth: 1,
            },
        ];
        let flow = ExecutionFlow {
            entry_point: entry.clone(),
            steps: steps.clone(),
        };
        assert_eq!(flow.entry_point, entry);
        assert_eq!(flow.steps.len(), 2);
    }

    #[test]
    fn execution_flow_equality_by_value() {
        let entry = FlowStep {
            name: "main".into(),
            kind: SymbolKind::Function,
            file: "src/main.rs".into(),
            line: 1,
            depth: 0,
        };
        let a = ExecutionFlow {
            entry_point: entry.clone(),
            steps: vec![entry.clone()],
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // -- BlastDirection tests -------------------------------------------------

    #[test]
    fn blast_direction_display() {
        assert_eq!(BlastDirection::Upstream.to_string(), "upstream");
        assert_eq!(BlastDirection::Downstream.to_string(), "downstream");
    }

    #[test]
    fn blast_direction_from_str() {
        assert_eq!(
            BlastDirection::from_str("upstream").unwrap(),
            BlastDirection::Upstream
        );
        assert_eq!(
            BlastDirection::from_str("downstream").unwrap(),
            BlastDirection::Downstream
        );
        assert!(BlastDirection::from_str("invalid").is_err());
    }

    // -- BlastSeverity tests --------------------------------------------------

    #[test]
    fn blast_severity_display() {
        assert_eq!(BlastSeverity::WillBreak.to_string(), "WILL BREAK");
        assert_eq!(BlastSeverity::LikelyAffected.to_string(), "LIKELY AFFECTED");
        assert_eq!(
            BlastSeverity::MayNeedTesting.to_string(),
            "MAY NEED TESTING"
        );
    }

    // -- BlastRiskLevel tests -------------------------------------------------

    #[test]
    fn blast_risk_level_display() {
        assert_eq!(BlastRiskLevel::Low.to_string(), "LOW");
        assert_eq!(BlastRiskLevel::Medium.to_string(), "MEDIUM");
        assert_eq!(BlastRiskLevel::High.to_string(), "HIGH");
        assert_eq!(BlastRiskLevel::Critical.to_string(), "CRITICAL");
    }

    // -- BlastAffectedSymbol tests --------------------------------------------

    #[test]
    fn blast_affected_symbol_creation() {
        let s = BlastAffectedSymbol {
            name: "handlePayment".into(),
            kind: SymbolKind::Function,
            file: "src/billing.ts".into(),
            line: 42,
            depth: 1,
            confidence: 0.85,
        };
        assert_eq!(s.name, "handlePayment");
        assert_eq!(s.kind, SymbolKind::Function);
        assert_eq!(s.file, "src/billing.ts");
        assert_eq!(s.line, 42);
        assert_eq!(s.depth, 1);
        assert!((s.confidence - 0.85).abs() < 1e-9);
    }

    // -- BlastTier tests ------------------------------------------------------

    #[test]
    fn blast_tier_creation() {
        let tier = BlastTier {
            severity: BlastSeverity::WillBreak,
            symbols: vec![BlastAffectedSymbol {
                name: "foo".into(),
                kind: SymbolKind::Function,
                file: "a.rs".into(),
                line: 1,
                depth: 1,
                confidence: 0.9,
            }],
        };
        assert_eq!(tier.severity, BlastSeverity::WillBreak);
        assert_eq!(tier.symbols.len(), 1);
    }

    // -- BlastAnalysis tests --------------------------------------------------

    #[test]
    fn blast_analysis_creation() {
        let analysis = BlastAnalysis {
            target: "processPayment".into(),
            direction: BlastDirection::Upstream,
            risk_level: BlastRiskLevel::Low,
            total_affected: 2,
            tiers: vec![],
            affected_files: vec!["src/billing.ts".into()],
        };
        assert_eq!(analysis.target, "processPayment");
        assert_eq!(analysis.direction, BlastDirection::Upstream);
        assert_eq!(analysis.risk_level, BlastRiskLevel::Low);
        assert_eq!(analysis.total_affected, 2);
        assert!(analysis.tiers.is_empty());
        assert_eq!(analysis.affected_files.len(), 1);
    }

    // -- ChangeScope tests ---------------------------------------------------

    #[test]
    fn change_scope_display_unstaged() {
        assert_eq!(ChangeScope::Unstaged.to_string(), "unstaged");
    }

    #[test]
    fn change_scope_display_staged() {
        assert_eq!(ChangeScope::Staged.to_string(), "staged");
    }

    #[test]
    fn change_scope_display_all() {
        assert_eq!(ChangeScope::All.to_string(), "all");
    }

    #[test]
    fn change_scope_display_compare() {
        assert_eq!(
            ChangeScope::Compare("main".into()).to_string(),
            "compare(main)"
        );
    }

    #[test]
    fn change_scope_equality() {
        assert_eq!(ChangeScope::Unstaged, ChangeScope::Unstaged);
        assert_eq!(
            ChangeScope::Compare("main".into()),
            ChangeScope::Compare("main".into())
        );
        assert_ne!(ChangeScope::Unstaged, ChangeScope::Staged);
        assert_ne!(
            ChangeScope::Compare("main".into()),
            ChangeScope::Compare("dev".into())
        );
    }

    #[test]
    fn change_analysis_creation() {
        let analysis = ChangeAnalysis {
            scope: ChangeScope::Unstaged,
            changed_symbols: vec![ChangedSymbol {
                name: "foo".into(),
                kind: SymbolKind::Function,
                file: "a.rs".into(),
                line: 1,
                change_type: ChangeType::Modified,
            }],
        };
        assert_eq!(analysis.scope, ChangeScope::Unstaged);
        assert_eq!(analysis.changed_symbols.len(), 1);
        assert_eq!(analysis.changed_symbols[0].name, "foo");
    }

    #[test]
    fn change_analysis_empty() {
        let analysis = ChangeAnalysis {
            scope: ChangeScope::Staged,
            changed_symbols: vec![],
        };
        assert!(analysis.changed_symbols.is_empty());
    }

    #[test]
    fn blast_analysis_equality() {
        let a = BlastAnalysis {
            target: "foo".into(),
            direction: BlastDirection::Downstream,
            risk_level: BlastRiskLevel::Medium,
            total_affected: 5,
            tiers: vec![],
            affected_files: vec![],
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // -- BlastRiskLevel ordering tests (TASK-072) ----------------------------

    #[test]
    fn blast_risk_level_ordering() {
        assert!(BlastRiskLevel::Low < BlastRiskLevel::Medium);
        assert!(BlastRiskLevel::Medium < BlastRiskLevel::High);
        assert!(BlastRiskLevel::High < BlastRiskLevel::Critical);
    }
}
