//! Signal-based reranking pipeline (TASK-092, DR-037).
//!
//! A signal is a stateless, pure function of (query, candidate, shared
//! context) returning a normalized contribution in `[0, 1]`. The pipeline
//! multiplies each contribution by its configured weight, sums the results,
//! and sorts by descending score with `(file, line)` tie-breaks.
//!
//! Cost contract (PRD-RANK-REQ-003): a signal whose weight is zero is
//! SKIPPED entirely — it is never evaluated and it contributes nothing to
//! the shared-context requirements, so zero-weight signals cost nothing.
//!
//! Grouping decision: the pipeline path buckets scored results by category
//! in tier order (`ranker::bucket_by_category`) before the ONE shared
//! dedup/group pass, so each category is emitted exactly once under any
//! valid weight configuration.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

use rusqlite::Connection;

use crate::ranker::{ClassifiedResult, ResultCategory};

/// The query side of a signal's input: everything about the invocation that
/// is not a candidate (PRD-RANK-REQ-002 — pure function of candidate +
/// shared query context).
pub struct QueryInfo<'a> {
    /// The raw search pattern as typed.
    pub pattern: &'a str,
}

/// Which slices of shared context a signal needs. The pipeline prepares
/// only what at least one ACTIVE (non-zero-weight) signal requires, batched
/// once per candidate set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextReqs {
    pub(crate) query_terms: bool,
    pub(crate) path_class: bool,
    pub(crate) symbol_hits: bool,
    pub(crate) lexical_scores: bool,
    pub(crate) embeddings: bool,
    pub(crate) file_churn: bool,
    pub(crate) co_change: bool,
}

impl ContextReqs {
    /// Require nothing: the pipeline then prepares no context at all.
    pub fn none() -> Self {
        Self::default()
    }

    /// Require the tokenized query terms.
    pub fn with_query_terms(mut self) -> Self {
        self.query_terms = true;
        self
    }

    /// Require per-file path classification (test vs ordinary).
    pub fn with_path_class(mut self) -> Self {
        self.path_class = true;
        self
    }

    /// Require the symbol-hit table for candidate (file, line) positions.
    pub fn with_symbol_hits(mut self) -> Self {
        self.symbol_hits = true;
        self
    }

    /// Require per-file BM25 scores normalized against the candidate set.
    pub fn with_lexical_scores(mut self) -> Self {
        self.lexical_scores = true;
        self
    }

    /// Require the query and candidate embedding vectors.
    pub fn with_embeddings(mut self) -> Self {
        self.embeddings = true;
        self
    }

    /// Require the mined per-file churn scores (TASK-096).
    pub fn with_file_churn(mut self) -> Self {
        self.file_churn = true;
        self
    }

    /// Require the mined co-change couplings between candidate files
    /// (TASK-097).
    pub fn with_co_change(mut self) -> Self {
        self.co_change = true;
        self
    }

    fn union(self, other: Self) -> Self {
        Self {
            query_terms: self.query_terms || other.query_terms,
            path_class: self.path_class || other.path_class,
            symbol_hits: self.symbol_hits || other.symbol_hits,
            lexical_scores: self.lexical_scores || other.lexical_scores,
            embeddings: self.embeddings || other.embeddings,
            file_churn: self.file_churn || other.file_churn,
            co_change: self.co_change || other.co_change,
        }
    }
}

/// Path classification bucket (TASK-094, PRD-RANK-REQ-011): the graded
/// ladder a file's PATH character demotes it through. Values are absolute
/// and strictly positive — 0.0 means "no evidence" in this codebase, and
/// every file carries some path character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    /// A generated file whose hand-written same-named peer is verified in
    /// the index. The strongest demotion: the peer is the better answer.
    GeneratedShadowed,
    /// A generated file before shadowing is resolved against the index.
    /// Never reaches a score directly: `prepare_context` (and every sort
    /// site) rewrites it to [`PathClass::GeneratedShadowed`] when a peer
    /// exists, else back to [`PathClass::Ordinary`] — a generated file is
    /// never demoted without a peer.
    Generated,
    /// Test directories, `*_test` stems, `.test.`/`.spec.` names.
    Test,
    /// Ambient type declarations: `.d.ts`/`.d.mts`/`.d.cts`, C headers.
    TypeDeclaration,
    /// Compatibility shims and deprecated compatibility layers.
    Shim,
    /// Examples, samples, fixtures, benchmarks, documentation.
    Example,
    /// Re-export barrels: `index.*`, `mod.rs`, `lib.rs`, `__init__.py`.
    Barrel,
    /// Program entry points: `main.*`, `__main__.py`.
    ModuleEntry,
    /// A regular source file — the default.
    Ordinary,
}

/// The ladder's absolute constants, most-demoted first.
pub fn path_character_value(class: PathClass) -> f32 {
    match class {
        PathClass::GeneratedShadowed | PathClass::Generated => 0.10,
        PathClass::Test => 0.20,
        PathClass::TypeDeclaration => 0.30,
        PathClass::Shim => 0.45,
        PathClass::Example => 0.60,
        PathClass::Barrel => 0.70,
        PathClass::ModuleEntry => 0.80,
        PathClass::Ordinary => 1.00,
    }
}

/// Whether a path is an ambient type declaration (`.d.ts`, `.d.mts`,
/// `.d.cts`) or a C header (`.h`). THE shared heuristic — the
/// `has_impl_exact` hints and the TypeDeclaration bucket both read it, so
/// the ".d.ts is not an implementation" judgment exists exactly once.
pub fn is_type_declaration(path: &str) -> bool {
    path.ends_with(".d.ts")
        || path.ends_with(".d.mts")
        || path.ends_with(".d.cts")
        || path.ends_with(".h")
}

/// Code-generation name markers: explicit `.generated.`/`.gen.` segments,
/// protobuf `_pb2`/`_pb2_grpc` stems, `_generated` stems, and the
/// codegen double extensions `.pb.go`/`.g.dart`/`.g.ts`.
pub fn is_generated_name(path: &str) -> bool {
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if name.contains(".generated.") || name.contains(".gen.") {
        return true;
    }
    if name.ends_with(".pb.go") || name.ends_with(".g.dart") || name.ends_with(".g.ts") {
        return true;
    }
    match std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
    {
        Some(stem) => {
            stem.ends_with("_pb2_grpc") || stem.ends_with("_pb2") || stem.ends_with("_generated")
        }
        None => false,
    }
}

/// The hand-written peer's file name for a generated name: strip the
/// generation marker and keep the extension
/// (`user.g.dart` → `user.dart`, `foo_pb2.py` → `foo.py`). None when the
/// name carries no marker.
pub fn strip_generated_marker(file_name: &str) -> Option<String> {
    let dot = file_name.rfind('.')?;
    let (stem, ext) = file_name.split_at(dot);
    let marker_stripped = [
        "_pb2_grpc",
        "_pb2",
        "_generated",
        ".generated",
        ".gen",
        ".pb",
        ".g",
    ]
    .iter()
    .find_map(|marker| stem.strip_suffix(marker))?;
    Some(format!("{marker_stripped}{ext}"))
}

/// Classify a file's path character into the graded ladder
/// (PRD-RANK-REQ-011). First-match precedence in ladder order: a
/// generation marker wins first (its demotion survives only when a peer is
/// verified), then the specific path buckets, with the more specific
/// bucket listed earlier (Test before TypeDeclaration, Example before
/// Barrel).
///
/// Deliberate divergence from `ranker::is_test_file` (which stays the
/// FROZEN kind input per DR-037): `is_test_file` flattens
/// docs/examples/fixtures/bench paths into its test tier, while this
/// ladder grades them as Example — test-ness here is the three unambiguous
/// test signals only. The path signal re-derives test-ness rather than
/// reading the kind input so the demotion survives `kind = 0`
/// configurations.
pub fn classify_path_character(path: &Path) -> PathClass {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let parent = path.parent().unwrap_or(Path::new(""));

    let in_dir = |dirs: &[&str]| {
        parent
            .components()
            .any(|c| dirs.contains(&c.as_os_str().to_string_lossy().as_ref()))
    };

    if is_generated_name(file_name) {
        return PathClass::Generated;
    }
    if in_dir(&["test", "tests", "__tests__"])
        || stem.ends_with("_test")
        || file_name.contains(".test.")
        || file_name.contains(".spec.")
    {
        return PathClass::Test;
    }
    if is_type_declaration(file_name) {
        return PathClass::TypeDeclaration;
    }
    if in_dir(&["compat", "compatibility", "shims", "deprecated", "legacy"])
        || matches!(
            stem,
            "compat" | "shim" | "legacy" | "deprecated" | "polyfill"
        )
        || stem.ends_with("_compat")
        || stem.ends_with("_shim")
    {
        return PathClass::Shim;
    }
    if in_dir(&[
        "example",
        "examples",
        "samples",
        "demos",
        "fixtures",
        "bench",
        "benchmarks",
        "docs",
        "doc",
    ]) || file_name.contains(".example.")
    {
        return PathClass::Example;
    }
    const BARRELS: &[&str] = &[
        "index.ts",
        "index.tsx",
        "index.js",
        "index.jsx",
        "index.mjs",
        "index.cjs",
        "mod.rs",
        "lib.rs",
        "__init__.py",
        "exports.ts",
        "exports.js",
    ];
    if BARRELS.contains(&file_name) {
        return PathClass::Barrel;
    }
    const MODULE_ENTRIES: &[&str] = &[
        "main.rs",
        "main.go",
        "main.py",
        "main.js",
        "main.ts",
        "__main__.py",
    ];
    if MODULE_ENTRIES.contains(&file_name) {
        return PathClass::ModuleEntry;
    }
    PathClass::Ordinary
}

/// Bound parameters per IN-list statement (the repo's chunking convention,
/// as in reach.rs). Bundled SQLite allows 32766; 900 keeps every statement
/// well under any build's limit.
const IN_CHUNK: usize = 900;

/// Which of `files` are generated names shadowing a hand-written peer
/// VERIFIED IN THE INDEX (the `files` table) — never the candidate set:
/// the peer may simply not match the query.
///
/// A peer is a file with the marker-stripped name in the same directory
/// (`src/user.g.dart` ← `src/user.dart`) that is not itself generated.
/// Resolution queries exactly the wanted peer paths in IN_CHUNK-sized
/// batches against the `files` PRIMARY KEY — an indexed point lookup per
/// peer, no directory over-fetch (a LIKE-prefix scan cannot use the
/// BINARY-collated path index and reads the whole table per directory). A
/// failing prepare degrades to ONE full `files` scan. No connection or no
/// generated candidates → no demotion (the conservative branch of AC-2).
pub fn resolve_generated_shadowing(conn: Option<&Connection>, files: &[String]) -> HashSet<String> {
    let mut shadowed = HashSet::new();
    let Some(conn) = conn else {
        return shadowed;
    };

    // (candidate, expected peer path) for every unique Generated file.
    let mut wanted: Vec<(String, String)> = Vec::new();
    let mut seen = HashSet::new();
    for file in files {
        if !seen.insert(file.clone())
            || classify_path_character(Path::new(file)) != PathClass::Generated
        {
            continue;
        }
        let path = Path::new(file);
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(peer_name) = strip_generated_marker(name) else {
            continue;
        };
        let prefix = match path.parent().and_then(|p| p.to_str()) {
            Some(dir) if !dir.is_empty() => format!("{dir}/"),
            _ => String::new(),
        };
        wanted.push((file.clone(), format!("{prefix}{peer_name}")));
    }
    if wanted.is_empty() {
        return shadowed;
    }

    // The exact wanted peer paths, in chunks, against the `files` primary
    // key: an indexed point lookup per peer, fetching nothing else.
    let peers: Vec<&String> = wanted.iter().map(|(_, peer)| peer).collect();
    let mut found: HashSet<String> = HashSet::new();
    let mut resolved = false;
    for chunk in peers.chunks(IN_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!("SELECT path FROM files WHERE path IN ({placeholders})");
        if let Ok(mut stmt) = conn.prepare(&sql)
            && let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                row.get::<_, String>(0)
            })
        {
            found.extend(rows.flatten());
            resolved = true;
        }
    }
    // Schema-mismatch fallback (kept from the LIKE form): an index whose
    // schema rejects the chunked statement still gets ONE full `files`
    // scan, so verification degrades the same conservative way instead of
    // silently demoting nothing.
    if !resolved
        && let Ok(mut stmt) = conn.prepare("SELECT path FROM files")
        && let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0))
    {
        found.extend(rows.flatten());
    }

    for (file, peer) in wanted {
        // The peer must be indexed, and must itself be hand-written.
        if found.contains(&peer) && !is_generated_name(&peer) {
            shadowed.insert(file);
        }
    }
    shadowed
}

/// Classify every unique file, resolving the Generated variant against the
/// index — THE one path-character judgment. The signal's context seeding
/// and every symbol-lookup sort site read it, so the ladder and its
/// never-demote-without-a-peer rule exist exactly once.
fn classify_paths(files: &[String], conn: Option<&Connection>) -> HashMap<String, PathClass> {
    let shadowed = resolve_generated_shadowing(conn, files);
    let mut classes: HashMap<String, PathClass> = HashMap::new();
    for file in files {
        let class = match classify_path_character(Path::new(file)) {
            PathClass::Generated if shadowed.contains(file) => PathClass::GeneratedShadowed,
            PathClass::Generated => PathClass::Ordinary,
            class => class,
        };
        classes.entry(file.clone()).or_insert(class);
    }
    classes
}

/// The resolved ladder VALUES for a batch of files — the sort key the
/// symbol-lookup sites (mcp `wonk_sym`, `wonk show`, the DB symbol query)
/// demote by. Sort descending; a missing entry means Ordinary (1.0), never
/// a demotion.
pub fn path_character_values(files: &[String], conn: Option<&Connection>) -> HashMap<String, f32> {
    classify_paths(files, conn)
        .into_iter()
        .map(|(file, class)| (file, path_character_value(class)))
        .collect()
}

/// A symbol definition located at a candidate position, with the number of
/// distinct indexed callers of that symbol name. The seam TASK-093's
/// centrality signal consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolHit {
    /// Symbol name as indexed.
    pub name: String,
    /// Symbol kind as indexed (e.g. "function").
    pub kind: String,
    /// Count of distinct callers referencing this symbol name.
    pub caller_count: u32,
}

/// Per-file BM25 scores for the candidate set, with the set-level bounds
/// folded once at preparation time so the lexical signal is O(1) per
/// candidate.
#[derive(Debug, Default, Clone)]
pub struct LexicalContext {
    pub(crate) scores: HashMap<String, f32>,
    pub(crate) min: f32,
    pub(crate) max: f32,
}

/// Embedding vectors for the query and the candidate positions.
#[derive(Debug, Default, Clone)]
pub struct EmbeddingContext {
    pub(crate) query: Option<Vec<f32>>,
    pub(crate) vectors: HashMap<(String, u64), Vec<f32>>,
}

/// Mined per-file churn scores for the candidate set, with the set max
/// folded once at preparation time (TASK-096).
#[derive(Debug, Default, Clone)]
pub struct ChurnContext {
    pub(crate) scores: HashMap<String, f32>,
    pub(crate) max: f32,
}

/// Set-relative co-change coupling for the candidate set (TASK-097): each
/// candidate's STRONGEST retained coupling to a file that is ALSO in the
/// response set, with the set max folded once at preparation time. Pairs
/// whose partner did not match the query are ignored — the signal reads
/// co-motion WITHIN the response, not absolute coupling.
#[derive(Debug, Default, Clone)]
pub struct CoChangeContext {
    pub(crate) best: HashMap<String, f32>,
    pub(crate) max: f32,
}

/// The query sources the pipeline prepares context against: the BM25
/// constants and the embedding provider kind from the loaded configuration.
/// Carrying them in one struct keeps `rank_and_explain`'s signature stable
/// while signals read whatever the user configured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextSources {
    /// BM25 constants (`[search] bm25_k1` / `bm25_b`).
    pub bm25: crate::bm25::Bm25Params,
    /// The configured embedding provider for the semantic signal.
    pub embedding: crate::embedding::EmbeddingProviderKind,
}

impl Default for ContextSources {
    fn default() -> Self {
        Self {
            bm25: crate::bm25::Bm25Params::from(&crate::config::SearchConfig::default()),
            embedding: crate::embedding::EmbeddingProviderKind::Bundled,
        }
    }
}

/// Context prepared once per candidate set and shared by all signals.
#[derive(Debug, Default, Clone)]
pub struct SharedContext {
    pub(crate) terms: Vec<String>,
    pub(crate) path_class: HashMap<String, PathClass>,
    pub(crate) symbol_hits: HashMap<(String, u64), SymbolHit>,
    pub(crate) lexical: LexicalContext,
    pub(crate) max_caller_count: u32,
    pub(crate) embeddings: EmbeddingContext,
    pub(crate) churn: ChurnContext,
    pub(crate) co_change: CoChangeContext,
}

impl SharedContext {
    /// Tokenized query terms (empty unless requested).
    pub fn terms(&self) -> &[String] {
        &self.terms
    }

    /// Path classification for a file (None unless requested).
    pub fn path_class(&self, file: &str) -> Option<PathClass> {
        self.path_class.get(file).copied()
    }

    /// Symbol hit at a candidate position (None unless requested).
    pub fn symbol_hit(&self, file: &str, line: u64) -> Option<&SymbolHit> {
        self.symbol_hits.get(&(file.to_string(), line))
    }

    /// Raw BM25 score for a candidate file (None unless lexical scores were
    /// requested and the index could produce them).
    pub fn lexical_score(&self, file: &str) -> Option<f32> {
        self.lexical.scores.get(file).copied()
    }

    /// `(min, max)` of the lexical scores across the candidate set; None
    /// when no scores were prepared.
    pub fn lexical_bounds(&self) -> Option<(f32, f32)> {
        if self.lexical.scores.is_empty() {
            None
        } else {
            Some((self.lexical.min, self.lexical.max))
        }
    }

    /// Largest caller count among the prepared symbol hits; 0 when none.
    pub fn max_caller_count(&self) -> u32 {
        self.max_caller_count
    }

    /// The embedded query vector, when semantic context was prepared.
    pub fn query_embedding(&self) -> Option<&[f32]> {
        self.embeddings.query.as_deref()
    }

    /// Candidate vector at a position (None unless prepared).
    pub fn embedding_at(&self, file: &str, line: u64) -> Option<&[f32]> {
        self.embeddings
            .vectors
            .get(&(file.to_string(), line))
            .map(|v| v.as_slice())
    }

    /// Mined churn score for a candidate file (None unless prepared).
    pub fn churn_score(&self, file: &str) -> Option<f32> {
        self.churn.scores.get(file).copied()
    }

    /// The largest churn score across the candidate set; 0 when none.
    pub fn max_churn(&self) -> f32 {
        self.churn.max
    }

    /// The candidate's strongest retained coupling to another file IN THE
    /// RESPONSE SET (None unless prepared, or when it has none).
    pub fn co_change_coupling(&self, file: &str) -> Option<f32> {
        self.co_change.best.get(file).copied()
    }

    /// The strongest candidate-set coupling; 0 when none.
    pub fn max_co_change(&self) -> f32 {
        self.co_change.max
    }
}

/// A stateless ranking signal (PRD-RANK-REQ-001/002).
pub trait Signal: Send + Sync {
    /// Stable identifier used in `[rank.weights]` and `--why` output.
    fn name(&self) -> &'static str;

    /// Which shared-context slices this signal needs.
    fn requires(&self) -> ContextReqs;

    /// Normalized contribution in `[0, 1]` for one candidate. Must be pure.
    fn contribution(
        &self,
        query: &QueryInfo<'_>,
        candidate: &ClassifiedResult,
        ctx: &SharedContext,
    ) -> f32;
}

/// One signal's contribution to one result, retained for explainability
/// (PRD-RANK-REQ-004): `value` is the UNWEIGHTED contribution.
#[derive(Debug, Clone, PartialEq)]
pub struct Contribution {
    /// Signal name.
    pub signal: &'static str,
    /// Unweighted normalized contribution.
    pub value: f32,
    /// Configured weight applied.
    pub weight: f32,
    /// `value * weight` as summed into the score.
    pub weighted: f32,
}

/// A classified result carrying its pipeline score and per-signal breakdown.
#[derive(Debug, Clone)]
pub struct ScoredResult {
    /// The classified candidate (classification stays in `ranker.rs`).
    pub classified: ClassifiedResult,
    /// Weighted sum of contributions.
    pub score: f32,
    /// Per-signal breakdown in registry order.
    pub contributions: Vec<Contribution>,
}

impl crate::ranker::GroupedItem for ScoredResult {
    fn category(&self) -> ResultCategory {
        self.classified.category
    }
    fn annotation(&self) -> Option<&str> {
        self.classified.annotation.as_deref()
    }
    fn set_annotation(&mut self, annotation: String) {
        self.classified.annotation = Some(annotation);
    }
}

/// The kind signal: ports the legacy category tier ordering into the
/// pipeline (PRD-RANK-REQ-010). Reads the category already present on the
/// candidate — it never re-classifies.
pub(crate) struct KindSignal;

impl Signal for KindSignal {
    fn name(&self) -> &'static str {
        "kind"
    }

    fn requires(&self) -> ContextReqs {
        ContextReqs::none()
    }

    fn contribution(
        &self,
        _query: &QueryInfo<'_>,
        candidate: &ClassifiedResult,
        _ctx: &SharedContext,
    ) -> f32 {
        kind_value(candidate.category)
    }
}

/// Kind contribution = `(5 - tier) / 5` as exact constants.
///
/// Strictly monotone in the legacy tier order, so score-descending with
/// `(file, line)` tie-breaks reproduces the legacy lexicographic
/// `(tier, file, line)` sort exactly — including duplicate-(file,line)
/// stability, since equal tiers give bitwise-identical constants and the
/// pipeline sorts stably.
pub fn kind_value(category: ResultCategory) -> f32 {
    match category {
        ResultCategory::Definition => 1.0,
        ResultCategory::CallSite => 0.8,
        ResultCategory::Import => 0.6,
        ResultCategory::Other => 0.4,
        ResultCategory::Comment => 0.2,
        ResultCategory::Test => 0.0,
    }
}

/// Min-max normalize `score` against the set bounds `[min, max]`.
///
/// A degenerate range (`max <= min` — an all-equal or all-zero set) yields
/// 0.0 for every member: with no contrast in the set there is no signal to
/// add. Out-of-range inputs clamp to `[0, 1]`; non-finite scores, bounds,
/// or results normalize to 0.0 rather than propagating NaN.
pub fn min_max_normalize(score: f32, min: f32, max: f32) -> f32 {
    if !score.is_finite() {
        return 0.0;
    }
    let range = max - min;
    if !range.is_finite() || range <= 0.0 {
        return 0.0;
    }
    let normalized = (score - min) / range;
    if normalized.is_finite() {
        normalized.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The lexical signal (TASK-093, PRD-RANK-REQ-001): the candidate file's
/// BM25 score over the tokenized query, min-max normalized across the
/// candidate set to maximize in-set contrast.
///
/// Line results inherit their file's score (the `rerank_lexical`
/// semantics), so two hits in one file always tie on this signal. A set
/// with no score contrast, a missing index, or a gated-off preparation all
/// contribute exactly 0.0.
pub(crate) struct LexicalSignal;

impl Signal for LexicalSignal {
    fn name(&self) -> &'static str {
        "lexical"
    }

    fn requires(&self) -> ContextReqs {
        ContextReqs::none().with_lexical_scores()
    }

    fn contribution(
        &self,
        _query: &QueryInfo<'_>,
        candidate: &ClassifiedResult,
        ctx: &SharedContext,
    ) -> f32 {
        let file = candidate.result.file.to_string_lossy();
        let (Some(score), Some((min, max))) = (ctx.lexical_score(&file), ctx.lexical_bounds())
        else {
            return 0.0;
        };
        min_max_normalize(score, min, max)
    }
}

/// Log-damped caller-count centrality (TASK-093, PRD-RANK-REQ-012):
/// `ln(1 + caller_count) / ln(1 + set_max)`.
///
/// The logarithm is the hub-dominance damper: 5 callers against a 500-caller
/// hub retain ~0.288 of the range where a linear ratio would crush them to
/// 0.010. An all-zero set has no denominator and scores 0.
pub fn centrality_value(caller_count: u32, set_max: u32) -> f32 {
    if set_max == 0 {
        return 0.0;
    }
    (1.0 + caller_count as f32).ln() / (1.0 + set_max as f32).ln()
}

/// The structural-centrality signal: how many distinct indexed callers
/// reference the symbol defined at the candidate position, log-damped
/// against the set maximum.
///
/// Candidates without a symbol hit (call sites, comments) are genuinely
/// un-called and contribute exactly 0 — a floor, never a penalty. Note the
/// TASK-092 seam: caller counts are keyed by symbol NAME, so same-named
/// symbols share a centrality.
pub(crate) struct CentralitySignal;

impl Signal for CentralitySignal {
    fn name(&self) -> &'static str {
        "centrality"
    }

    fn requires(&self) -> ContextReqs {
        ContextReqs::none().with_symbol_hits()
    }

    fn contribution(
        &self,
        _query: &QueryInfo<'_>,
        candidate: &ClassifiedResult,
        ctx: &SharedContext,
    ) -> f32 {
        let file = candidate.result.file.to_string_lossy();
        let Some(hit) = ctx.symbol_hit(&file, candidate.result.line) else {
            return 0.0;
        };
        centrality_value(hit.caller_count, ctx.max_caller_count())
    }
}

/// Log-damped churn against the candidate-set maximum (TASK-096,
/// PRD-HIST-REQ-006): `ln(1 + score) / ln(1 + set_max)` over the stored
/// age-weighted score — the same damper as [`centrality_value`], so a
/// hyper-active file cannot crush the rest of the set. The weighting
/// itself is baked at index time by `history::aggregate_churn`; the query
/// only normalizes. An all-dormant set (`set_max <= 0`) and non-finite
/// inputs score exactly 0.0 — absent is zero, never a penalty.
pub fn churn_value(score: f32, set_max: f32) -> f32 {
    if !score.is_finite() || !set_max.is_finite() || set_max <= 0.0 {
        return 0.0;
    }
    (1.0 + score).ln() / (1.0 + set_max).ln()
}

/// Log-damped co-change coupling against the candidate-set maximum
/// (TASK-097, PRD-HIST-REQ-006): `ln(1 + weight) / ln(1 + set_max)` over
/// the stored age-weighted co-occurrence — the same damper as
/// [`churn_value`], so one intensely-coupled pair cannot crush the rest
/// of the set. The signal is SET-RELATIVE: rerank only reorders matched
/// files, so "the query target" is the response set itself, and a
/// candidate scores by its strongest coupling to another file IN that
/// set. An uncoupled set (`set_max <= 0`), a single-file set, and
/// non-finite inputs score exactly 0.0 — a loner is never penalized
/// beyond its missing evidence.
pub fn co_change_value(weight: f32, set_max: f32) -> f32 {
    if !weight.is_finite() || !set_max.is_finite() || set_max <= 0.0 {
        return 0.0;
    }
    (1.0 + weight).ln() / (1.0 + set_max).ln()
}

/// The churn signal: how actively the candidate file was modified within
/// the mined commit window. Reads the `file_churn` aggregate prepared for
/// the candidate set; a file absent from the table (never touched in the
/// window, or no git history) contributes exactly 0.0.
pub(crate) struct ChurnSignal;

impl Signal for ChurnSignal {
    fn name(&self) -> &'static str {
        "churn"
    }

    fn requires(&self) -> ContextReqs {
        ContextReqs::none().with_file_churn()
    }

    fn contribution(
        &self,
        _query: &QueryInfo<'_>,
        candidate: &ClassifiedResult,
        ctx: &SharedContext,
    ) -> f32 {
        let file = candidate.result.file.to_string_lossy();
        match ctx.churn_score(&file) {
            Some(score) => churn_value(score, ctx.max_churn()),
            None => 0.0,
        }
    }
}

/// The MAXIMAL identifier runs (`[A-Za-z0-9_]+` bounded by
/// non-identifier characters) of `line`, lowercased — the ONE shared
/// scanner behind the prominence token tier and the proximity signal.
///
/// A dedicated scanner rather than `tokenizer::tokenize`: the tokenizer
/// splits on `_`, so it would token-match `foo` inside `foo_bar` — here the
/// boundary is the point, because a query term appearing inside a longer
/// identifier names a different symbol. Terms containing non-ASCII
/// characters can never match (code identifiers are ASCII runs).
pub fn identifier_tokens(line: &str) -> Vec<String> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|run| !run.is_empty())
        .map(|run| run.to_ascii_lowercase())
        .collect()
}

/// Whether `term` occurs in `line` as a maximal identifier run
/// (case-insensitive) — a non-empty scan over [`identifier_tokens`].
pub fn contains_identifier_token(line: &str, term: &str) -> bool {
    !term.is_empty()
        && identifier_tokens(line)
            .iter()
            .any(|t| t.eq_ignore_ascii_case(term))
}

/// How closely the query terms co-occur in `content` (TASK-094,
/// PRD-RANK-REQ-013): `1 / gap` over the FIRST-OCCURRENCE token indices of
/// the query terms present as whole identifier tokens — adjacent terms 1.0,
/// one token between 0.5, decaying hyperbolically with distance.
///
/// Fewer than two present terms (single-term queries, absent terms,
/// compound-name partials like `foo` inside `foo_bar`) contribute exactly
/// 0.0: with nothing to co-locate the signal is inert, never a penalty.
pub fn proximity_value(content: &str, terms: &[String]) -> f32 {
    let tokens = identifier_tokens(content);
    if tokens.is_empty() {
        return 0.0;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut first_indices: Vec<usize> = Vec::new();
    for term in terms {
        if term.is_empty() || !seen.insert(term.as_str()) {
            continue;
        }
        if let Some(index) = tokens.iter().position(|token| token == term) {
            first_indices.push(index);
        }
    }
    let Some(gap) = first_indices
        .iter()
        .copied()
        .max()
        .zip(first_indices.iter().copied().min())
        .map(|(max, min)| (max - min) as f32)
    else {
        return 0.0;
    };
    // Distinct whole-token terms sit at distinct indices, so gap >= 1; the
    // guard keeps a pathological zero gap from poisoning scores with inf.
    if gap >= 1.0 { 1.0 / gap } else { 0.0 }
}

/// The proximity signal: how closely the query terms co-occur in the
/// matched line. Reads only the tokenized query terms and the candidate's
/// matched text — no new context slice, no SQL.
pub(crate) struct ProximitySignal;

impl Signal for ProximitySignal {
    fn name(&self) -> &'static str {
        "proximity"
    }

    fn requires(&self) -> ContextReqs {
        ContextReqs::none().with_query_terms()
    }

    fn contribution(
        &self,
        _query: &QueryInfo<'_>,
        candidate: &ClassifiedResult,
        ctx: &SharedContext,
    ) -> f32 {
        proximity_value(&candidate.result.content, ctx.terms())
    }
}

/// Prominence tier constants (TASK-093, PRD-RANK-REQ-015).
pub const PROMINENCE_EXACT: f32 = 1.0;
/// A query term mentioned as a whole identifier in the matched line.
pub const PROMINENCE_TOKEN: f32 = 0.5;

/// The prominence signal: does this candidate DEFINE (or merely mention,
/// or merely contain) the queried name?
///
/// Tiers, best one wins: 1.0 exact definition — the symbol indexed at this
/// position has a name equal to a query term or to the trimmed raw pattern
/// (the raw pattern is required because tokenization splits `my_func` into
/// `[my, func]`, neither of which equals the indexed name); 0.5 token
/// mention — some term occurs in the matched line as a maximal identifier
/// run; 0.0 incidental — substring only, which grep already guarantees.
/// No query terms means no tiers: exactly 0.
pub(crate) struct ProminenceSignal;

impl Signal for ProminenceSignal {
    fn name(&self) -> &'static str {
        "prominence"
    }

    fn requires(&self) -> ContextReqs {
        ContextReqs::none().with_query_terms().with_symbol_hits()
    }

    fn contribution(
        &self,
        query: &QueryInfo<'_>,
        candidate: &ClassifiedResult,
        ctx: &SharedContext,
    ) -> f32 {
        let terms = ctx.terms();
        if terms.is_empty() {
            return 0.0;
        }
        let file = candidate.result.file.to_string_lossy();
        if let Some(hit) = ctx.symbol_hit(&file, candidate.result.line) {
            let raw = query.pattern.trim();
            let name_matches_term = terms
                .iter()
                .any(|term| term.eq_ignore_ascii_case(&hit.name));
            let name_matches_pattern = !raw.is_empty() && hit.name.eq_ignore_ascii_case(raw);
            if name_matches_term || name_matches_pattern {
                return PROMINENCE_EXACT;
            }
        }
        if terms
            .iter()
            .any(|term| contains_identifier_token(&candidate.result.content, term))
        {
            return PROMINENCE_TOKEN;
        }
        0.0
    }
}

/// Absolute cosine-to-contribution mapping (TASK-093):
/// `clamp01((cosine + 1) * 0.5)`.
///
/// Deliberately NOT set-relative: an absent embedding must stay strictly
/// below every present candidate, and min-max normalization would tie the
/// absent candidates with the worst present one — inverting the
/// zero-not-penalty contract. Non-finite input maps to 0.
pub fn semantic_value(cosine: f32) -> f32 {
    if !cosine.is_finite() {
        return 0.0;
    }
    ((cosine + 1.0) * 0.5).clamp(0.0, 1.0)
}

/// The semantic signal (PRD-RANK-REQ-001): cosine similarity between the
/// query embedding and the candidate's indexed embedding, both
/// L2-normalized, mapped absolutely per [`semantic_value`].
///
/// A missing query embedding or a candidate without a vector contributes
/// exactly 0.0 — zero, not a penalty — so a candidate the signal knows
/// nothing about is never demoted below its other signals' score.
pub(crate) struct SemanticSignal;

impl Signal for SemanticSignal {
    fn name(&self) -> &'static str {
        "semantic"
    }

    fn requires(&self) -> ContextReqs {
        ContextReqs::none().with_embeddings()
    }

    fn contribution(
        &self,
        _query: &QueryInfo<'_>,
        candidate: &ClassifiedResult,
        ctx: &SharedContext,
    ) -> f32 {
        let Some(query) = ctx.query_embedding() else {
            return 0.0;
        };
        let file = candidate.result.file.to_string_lossy();
        let Some(vector) = ctx.embedding_at(&file, candidate.result.line) else {
            return 0.0;
        };
        semantic_value(crate::semantic::dot_product(query, vector))
    }
}

/// The path-character signal (TASK-094, PRD-RANK-REQ-011): the candidate
/// file's graded ladder value — generated-shadowed < test < type
/// declaration < shim < example < barrel < module entry < ordinary.
///
/// Graded, never exclusion: a test file still scores strictly above zero,
/// so a test file that is the best answer on the other signals still
/// ranks. The signal re-derives test-ness from the path (it does not read
/// the kind input) so the demotion survives `kind = 0` configurations;
/// under `kind > 0` both signals demote test files and agree.
pub(crate) struct PathCharacterSignal;

impl Signal for PathCharacterSignal {
    fn name(&self) -> &'static str {
        "path_character"
    }

    fn requires(&self) -> ContextReqs {
        ContextReqs::none().with_path_class()
    }

    fn contribution(
        &self,
        _query: &QueryInfo<'_>,
        candidate: &ClassifiedResult,
        ctx: &SharedContext,
    ) -> f32 {
        let file = candidate.result.file.to_string_lossy();
        match ctx.path_class(&file) {
            Some(class) => path_character_value(class),
            // No classification, no demotion.
            None => path_character_value(PathClass::Ordinary),
        }
    }
}

/// Whether the query resembles a type or function signature
/// (TASK-094, PRD-RANK-REQ-014): a parenthesis, an arrow, or a
/// path-separator — punctuation a name-shaped query would not carry.
pub fn is_signature_query(pattern: &str) -> bool {
    let trimmed = pattern.trim();
    trimmed.contains('(') || trimmed.contains("->") || trimmed.contains("::")
}

/// The query classes TASK-095 detects (PRD-RANK-REQ-007): which shape a
/// query has, deciding how the lexical/semantic blend is scaled.
///
/// Misclassification blast radius is one wrong blend — never a wrong signal
/// set (DR-038): classification only scales the two content channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum QueryClass {
    /// A single identifier-shaped token: a name lookup
    /// (`validateToken`, `my_func`, `cache`).
    Symbol,
    /// A file-path-shaped fragment (`internal/auth/token.go`).
    Path,
    /// Signature-shaped: carries `(`, `->`, or `::`
    /// (`parse(input: &str)`, `Foo::bar`).
    Signature,
    /// Everything else: multi-word natural language, empty, punctuation
    /// prose. The neutral 1.0 baseline (REQ-009).
    Conceptual,
}

impl QueryClass {
    /// The stable kebab-case name used by `--query-class`, MCP, and output.
    pub fn as_str(&self) -> &'static str {
        match self {
            QueryClass::Symbol => "symbol",
            QueryClass::Path => "path",
            QueryClass::Signature => "signature",
            QueryClass::Conceptual => "conceptual",
        }
    }
}

impl std::fmt::Display for QueryClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for QueryClass {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for class in [
            QueryClass::Symbol,
            QueryClass::Path,
            QueryClass::Signature,
            QueryClass::Conceptual,
        ] {
            if class.as_str() == s {
                return Ok(class);
            }
        }
        Err(format!(
            "unknown query class '{s}' (known: symbol, path, signature, conceptual)"
        ))
    }
}

/// Whether the pattern is one identifier-shaped token: non-empty and every
/// character `[A-Za-z0-9_]` (camelCase, snake_case, PascalCase, SCREAMING,
/// and bare lowercase words alike — a no-space single token is a name
/// lookup, not prose).
pub fn is_identifier_shaped(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Classify a query by shape (PRD-RANK-REQ-007), first-match:
///
/// 1. **Signature** — [`is_signature_query`] verbatim;
/// 2. **Path** — contains `/` or `\`;
/// 3. **Symbol** — [`is_identifier_shaped`] single token;
/// 4. **Conceptual** — everything else (the default, REQ-009's neutral).
pub fn classify_query(pattern: &str) -> QueryClass {
    let trimmed = pattern.trim();
    if is_signature_query(trimmed) {
        return QueryClass::Signature;
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return QueryClass::Path;
    }
    if is_identifier_shaped(trimmed) {
        return QueryClass::Symbol;
    }
    QueryClass::Conceptual
}

/// Keywords a definition line opens with, across the indexed languages.
const DEFINITION_KEYWORDS: &[&str] = &[
    "fn",
    "def",
    "func",
    "function",
    "class",
    "struct",
    "enum",
    "interface",
    "trait",
    "impl",
    "type",
    "pub",
    "async",
    "const",
    "static",
    "export",
    "module",
    "namespace",
];

/// The signature-match contribution (TASK-094, PRD-RANK-REQ-014):
///
/// - 1.0 the candidate's category is Definition (index-backed);
/// - 0.5 the matched line is definition-SHAPED — it contains a
///   parenthesis AND a definition keyword among its first three
///   identifier tokens (definitions announce themselves);
/// - 0.0 otherwise.
///
/// Inert unless the query itself is signature-shaped: a name query must
/// not reorder through this signal at all, so [`is_signature_query`] gates
/// everything.
pub fn signature_value(query_shaped: bool, category: ResultCategory, content: &str) -> f32 {
    if !query_shaped {
        return 0.0;
    }
    if category == ResultCategory::Definition {
        return 1.0;
    }
    if content.contains('(')
        && identifier_tokens(content)
            .iter()
            .take(3)
            .any(|token| DEFINITION_KEYWORDS.contains(&token.as_str()))
    {
        return 0.5;
    }
    0.0
}

/// The signature-match signal: definition lines answer signature-shaped
/// queries. Reads only the raw pattern, the candidate's category, and its
/// matched text — no context, no SQL.
pub(crate) struct SignatureSignal;

impl Signal for SignatureSignal {
    fn name(&self) -> &'static str {
        "signature"
    }

    fn requires(&self) -> ContextReqs {
        ContextReqs::none()
    }

    fn contribution(
        &self,
        query: &QueryInfo<'_>,
        candidate: &ClassifiedResult,
        _ctx: &SharedContext,
    ) -> f32 {
        signature_value(
            is_signature_query(query.pattern),
            candidate.category,
            &candidate.result.content,
        )
    }
}

/// Registry of built-in signals. TASK-093/094 append entries here; config
/// name validation derives from this list, so new signals are accepted by
/// `[rank.weights]` automatically.
pub fn builtin_signals() -> Vec<Box<dyn Signal>> {
    vec![
        Box::new(KindSignal),
        Box::new(LexicalSignal),
        Box::new(SemanticSignal),
        Box::new(CentralitySignal),
        Box::new(ProminenceSignal),
        Box::new(PathCharacterSignal),
        Box::new(ProximitySignal),
        Box::new(SignatureSignal),
        Box::new(ChurnSignal),
    ]
}

/// Valid signal names, derived from the registry (single source of truth).
pub fn known_signal_names() -> Vec<&'static str> {
    builtin_signals().iter().map(|s| s.name()).collect()
}

/// Validated per-signal weights. Absent names weigh zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WeightTable {
    weights: HashMap<String, f32>,
}

impl WeightTable {
    /// Validate a configured weight map against the signal registry.
    ///
    /// Unknown names are a hard error naming the offender and the valid
    /// names (PRD-RANK-REQ-006); non-finite values (NaN, ±inf) are rejected
    /// because they would poison every downstream score.
    pub fn from_config(weights: &HashMap<String, f32>) -> anyhow::Result<Self> {
        let known = known_signal_names();
        for (name, value) in weights {
            if !known.contains(&name.as_str()) {
                anyhow::bail!(
                    "unknown signal name '{}' in [rank.weights] (known: {})",
                    name,
                    known.join(", ")
                );
            }
            if !value.is_finite() {
                anyhow::bail!(
                    "non-finite weight {} for signal '{}' in [rank.weights]",
                    value,
                    name
                );
            }
        }
        Ok(Self {
            weights: weights.clone(),
        })
    }

    /// Build a table from explicit name/weight pairs without registry
    /// validation, for programmatic callers that pair the weights with
    /// their own signal list (tests use this for spy signals). Finiteness
    /// is still enforced; config files must go through `from_config`.
    pub fn from_pairs<I: IntoIterator<Item = (String, f32)>>(entries: I) -> anyhow::Result<Self> {
        let mut weights = HashMap::new();
        for (name, value) in entries {
            if !value.is_finite() {
                anyhow::bail!("non-finite weight {value} for signal '{name}'");
            }
            weights.insert(name, value);
        }
        Ok(Self { weights })
    }

    /// Weight for a signal name; zero when absent.
    pub fn weight(&self, name: &str) -> f32 {
        self.weights.get(name).copied().unwrap_or(0.0)
    }

    /// The kind-only default: `{kind = 1.0}`.
    pub fn kind_dominant() -> Self {
        Self {
            weights: HashMap::from([("kind".to_string(), 1.0)]),
        }
    }
}

/// Union of the context requirements of the ACTIVE (non-zero-weight)
/// signals. Zero-weight signals are excluded here as well, so they cost
/// nothing to prepare for either (PRD-RANK-REQ-003).
pub(crate) fn union_reqs(signals: &[Box<dyn Signal>], weights: &WeightTable) -> ContextReqs {
    signals
        .iter()
        .filter(|s| weights.weight(s.name()) != 0.0)
        .map(|s| s.requires())
        .fold(ContextReqs::none(), ContextReqs::union)
}

/// Prepare the shared context for a candidate set, batched once.
///
/// Short-circuits to an empty context when `reqs` is default (no SQL runs,
/// no tokenization happens) and leaves `symbol_hits` empty when no
/// connection is available.
pub fn prepare_context(
    reqs: ContextReqs,
    pattern: &str,
    results: &[ClassifiedResult],
    conn: Option<&Connection>,
    sources: &ContextSources,
) -> SharedContext {
    let mut ctx = SharedContext::default();
    if reqs.query_terms {
        ctx.terms = crate::tokenizer::tokenize(pattern);
    }
    if reqs.path_class {
        let files: Vec<String> = results
            .iter()
            .map(|r| r.result.file.to_string_lossy().into_owned())
            .collect();
        ctx.path_class = classify_paths(&files, conn);
    }
    if reqs.symbol_hits
        && let Some(conn) = conn
    {
        ctx.symbol_hits = load_symbol_hits(conn, results);
        ctx.max_caller_count = ctx
            .symbol_hits
            .values()
            .map(|hit| hit.caller_count)
            .max()
            .unwrap_or(0);
    }
    if reqs.lexical_scores
        && let Some(conn) = conn
    {
        let files: std::collections::HashSet<String> = results
            .iter()
            .map(|r| r.result.file.to_string_lossy().into_owned())
            .collect();
        if let Some(scores) = crate::bm25::file_bm25_scores(conn, &files, pattern, sources.bm25)
            && !scores.is_empty()
        {
            let min = scores.values().copied().fold(f32::INFINITY, f32::min);
            let max = scores.values().copied().fold(f32::NEG_INFINITY, f32::max);
            ctx.lexical = LexicalContext { scores, min, max };
        }
    }
    if reqs.embeddings
        && let Some(conn) = conn
    {
        ctx.embeddings = prepare_embeddings(conn, pattern, results, sources.embedding);
    }
    if reqs.file_churn
        && let Some(conn) = conn
    {
        let files: std::collections::HashSet<String> = results
            .iter()
            .map(|r| r.result.file.to_string_lossy().into_owned())
            .collect();
        ctx.churn = load_churn_scores(conn, &files);
    }
    if reqs.co_change
        && let Some(conn) = conn
    {
        let files: std::collections::HashSet<String> = results
            .iter()
            .map(|r| r.result.file.to_string_lossy().into_owned())
            .collect();
        ctx.co_change = load_co_change_scores(conn, &files);
    }
    ctx
}

/// Load the churn scores for exactly the candidate files, in IN_CHUNK
/// batches against the `file_churn` primary key, folding the set max once.
///
/// A presence probe (the bm25 precedent) degrades to an empty context on a
/// pre-TASK-096 index whose `file_churn` table does not exist; every
/// prepare failure is the same zero-path, never an error.
fn load_churn_scores(conn: &Connection, files: &std::collections::HashSet<String>) -> ChurnContext {
    let mut ctx = ChurnContext::default();
    if files.is_empty() {
        return ctx;
    }
    if conn
        .query_row("SELECT 1 FROM file_churn LIMIT 1", [], |_| Ok(()))
        .is_err()
    {
        return ctx;
    }

    let mut wanted: Vec<&String> = files.iter().collect();
    wanted.sort_unstable();
    for chunk in wanted.chunks(IN_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!("SELECT file, score FROM file_churn WHERE file IN ({placeholders})");
        let Ok(mut stmt) = conn.prepare(&sql) else {
            continue;
        };
        let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f32>(1)?))
        }) else {
            continue;
        };
        for row in rows.flatten() {
            ctx.scores.insert(row.0, row.1);
        }
    }
    ctx.max = ctx
        .scores
        .values()
        .copied()
        .filter(|s| s.is_finite())
        .fold(0.0f32, f32::max);
    ctx
}

/// Load the set-relative couplings for exactly the candidate files, in
/// IN_CHUNK batches against the `co_change` primary key. Only rows whose
/// `file_b` is ALSO a candidate are kept, folding each `file_a`'s maximum
/// and the set maximum once — the response set is the "query target" the
/// signal reads (PRD-HIST-REQ-006).
///
/// A presence probe (the bm25/file_churn precedent) degrades to an empty
/// context on a pre-TASK-097 index whose `co_change` table does not exist;
/// every prepare failure is the same zero-path, never an error.
fn load_co_change_scores(
    conn: &Connection,
    files: &std::collections::HashSet<String>,
) -> CoChangeContext {
    let mut ctx = CoChangeContext::default();
    if files.is_empty() {
        return ctx;
    }
    if conn
        .query_row("SELECT 1 FROM co_change LIMIT 1", [], |_| Ok(()))
        .is_err()
    {
        return ctx;
    }

    let mut wanted: Vec<&String> = files.iter().collect();
    wanted.sort_unstable();
    for chunk in wanted.chunks(IN_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql =
            format!("SELECT file_a, file_b, weight FROM co_change WHERE file_a IN ({placeholders})");
        let Ok(mut stmt) = conn.prepare(&sql) else {
            continue;
        };
        let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f32>(2)?,
            ))
        }) else {
            continue;
        };
        for (file_a, file_b, weight) in rows.flatten() {
            if !files.contains(&file_b) || !weight.is_finite() {
                continue;
            }
            ctx.best
                .entry(file_a)
                .and_modify(|best| *best = best.max(weight))
                .or_insert(weight);
        }
    }
    ctx.max = ctx
        .best
        .values()
        .copied()
        .filter(|w| w.is_finite())
        .fold(0.0f32, f32::max);
    ctx
}

/// Prepare the semantic context: candidate vectors first, then the query
/// embedding, exactly once (TASK-093).
///
/// Every failure is a zero-path, never an error — a supplementary signal
/// must not be able to fail a search whose other sources are healthy: a
/// provider plan error (foreign vector space) yields an empty context; no
/// candidate vectors skips the query embed entirely; an embed failure or a
/// corrupt row yields an empty context. Fallback warnings from the plan
/// are swallowed here (the primary semantic path already warns); tracked
/// for TASK-095.
fn prepare_embeddings(
    conn: &Connection,
    pattern: &str,
    results: &[ClassifiedResult],
    configured: crate::embedding::EmbeddingProviderKind,
) -> EmbeddingContext {
    let mut ctx = EmbeddingContext::default();
    let Ok(plan) = crate::embedding::plan_query_provider(conn, configured) else {
        return ctx;
    };
    let provider = plan.provider;
    let positions: std::collections::HashSet<(String, u64)> = results
        .iter()
        .map(|r| (r.result.file.to_string_lossy().into_owned(), r.result.line))
        .collect();
    let Ok(vectors) =
        crate::embedding::load_embedding_vectors_at_positions(conn, &positions, provider.as_ref())
    else {
        return ctx;
    };
    if vectors.is_empty() {
        return ctx;
    }
    if let Ok(mut query) = provider.embed_single(pattern) {
        crate::embedding::normalize(&mut query);
        ctx.query = Some(query);
        ctx.vectors = vectors;
    }
    ctx
}

/// Batched symbol-hit lookup, filtered to the files present in the result
/// set (mirroring `ranker::IndexLookup`): two SQL queries total.
fn load_symbol_hits(
    conn: &Connection,
    results: &[ClassifiedResult],
) -> HashMap<(String, u64), SymbolHit> {
    let files: std::collections::HashSet<String> = results
        .iter()
        .map(|r| r.result.file.to_string_lossy().into_owned())
        .collect();
    let mut hits = HashMap::new();
    if files.is_empty() {
        return hits;
    }

    let positions: std::collections::HashSet<(String, u64)> = results
        .iter()
        .map(|r| (r.result.file.to_string_lossy().into_owned(), r.result.line))
        .collect();

    let placeholders: Vec<&str> = files.iter().map(|_| "?").collect();
    let in_clause = placeholders.join(", ");
    let file_params: Vec<String> = files.into_iter().collect();

    // Query 1: symbol rows for the candidate files, kept only at candidate
    // (file, line) positions.
    let sql_symbols =
        format!("SELECT file, line, name, kind FROM symbols WHERE file IN ({in_clause})");
    if let Ok(mut stmt) = conn.prepare(&sql_symbols) {
        let boxed: Vec<Box<dyn rusqlite::types::ToSql>> = file_params
            .iter()
            .map(|s| Box::new(s.clone()) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        let refs: Vec<&dyn rusqlite::types::ToSql> = boxed.iter().map(|b| b.as_ref()).collect();
        if let Ok(rows) = stmt.query_map(refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        }) {
            for (file, line, name, kind) in rows.flatten() {
                let key = (file, line as u64);
                if positions.contains(&key) {
                    hits.entry(key).or_insert_with(|| SymbolHit {
                        name,
                        kind,
                        caller_count: 0,
                    });
                }
            }
        }
    }

    // Query 2: distinct-caller counts per referenced name, folded into the
    // hits by symbol name. COUNT(DISTINCT caller_id) ignores NULL callers.
    let sql_callers = format!(
        "SELECT name, COUNT(DISTINCT caller_id) FROM \"references\" \
         WHERE file IN ({in_clause}) GROUP BY name"
    );
    if let Ok(mut stmt) = conn.prepare(&sql_callers) {
        let boxed: Vec<Box<dyn rusqlite::types::ToSql>> = file_params
            .iter()
            .map(|s| Box::new(s.clone()) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        let refs: Vec<&dyn rusqlite::types::ToSql> = boxed.iter().map(|b| b.as_ref()).collect();
        if let Ok(rows) = stmt.query_map(refs.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) {
            let callers: HashMap<String, u32> = rows
                .flatten()
                .map(|(name, count)| (name, count.max(0) as u32))
                .collect();
            for hit in hits.values_mut() {
                hit.caller_count = callers.get(&hit.name).copied().unwrap_or(0);
            }
        }
    }

    hits
}

/// Score and sort classified results with the built-in signal registry.
pub fn rerank(
    results: Vec<ClassifiedResult>,
    query: &QueryInfo<'_>,
    conn: Option<&Connection>,
    weights: &WeightTable,
    sources: &ContextSources,
) -> Vec<ScoredResult> {
    rerank_with_signals(builtin_signals(), results, query, conn, weights, sources)
}

/// `rerank` over an explicit signal list (the test seam for spy signals).
pub(crate) fn rerank_with_signals(
    signals: Vec<Box<dyn Signal>>,
    results: Vec<ClassifiedResult>,
    query: &QueryInfo<'_>,
    conn: Option<&Connection>,
    weights: &WeightTable,
    sources: &ContextSources,
) -> Vec<ScoredResult> {
    let active: Vec<&Box<dyn Signal>> = signals
        .iter()
        .filter(|s| weights.weight(s.name()) != 0.0)
        .collect();
    let reqs = union_reqs(&signals, weights);
    let ctx = prepare_context(reqs, query.pattern, &results, conn, sources);

    let mut scored: Vec<ScoredResult> = results
        .into_iter()
        .map(|classified| {
            let mut score = 0.0f32;
            let mut contributions = Vec::with_capacity(active.len());
            for signal in &active {
                let weight = weights.weight(signal.name());
                let value = signal.contribution(query, &classified, &ctx);
                let weighted = value * weight;
                score += weighted;
                contributions.push(Contribution {
                    signal: signal.name(),
                    value,
                    weight,
                    weighted,
                });
            }
            ScoredResult {
                classified,
                score,
                contributions,
            }
        })
        .collect();
    scored.sort_by(compare_scored);
    scored
}

/// Order scored results: score descending, then `(file, line)` ascending.
/// A NaN score compares Equal on the score axis (mirroring `fuse_rrf`), so
/// it falls through to the deterministic tie-breaks instead of panicking.
fn compare_scored(a: &ScoredResult, b: &ScoredResult) -> std::cmp::Ordering {
    b.score
        .partial_cmp(&a.score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.classified.result.file.cmp(&b.classified.result.file))
        .then_with(|| a.classified.result.line.cmp(&b.classified.result.line))
}

/// Scaling of the two content channels for one query class
/// (TASK-095, PRD-RANK-REQ-008). Multipliers adjust an ALREADY-CONFIGURED
/// weight — `0 × m = 0` — adjustment, not creation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelMultipliers {
    /// Multiplier applied to the `lexical` signal's weight.
    pub lexical: f32,
    /// Multiplier applied to the `semantic` signal's weight.
    pub semantic: f32,
}

impl Default for ChannelMultipliers {
    fn default() -> Self {
        Self::neutral()
    }
}

impl ChannelMultipliers {
    /// The neutral 1.0/1.0 scaling.
    pub fn neutral() -> Self {
        Self {
            lexical: 1.0,
            semantic: 1.0,
        }
    }
}

/// Per-class channel multipliers (TASK-095, PRD-RANK-REQ-008/009).
///
/// There is deliberately NO entry for the conceptual class: it is the
/// neutral 1.0 baseline, unconditionally, so neutrality cannot be
/// configured away (REQ-009) — the config parser hard-rejects a
/// `[rank.class_multipliers.conceptual]` table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClassMultipliers {
    /// Multipliers for symbol-shaped queries.
    pub symbol: ChannelMultipliers,
    /// Multipliers for path-shaped queries.
    pub path: ChannelMultipliers,
    /// Multipliers for signature-shaped queries.
    pub signature: ChannelMultipliers,
}

impl Default for ClassMultipliers {
    fn default() -> Self {
        Self::neutral()
    }
}

impl ClassMultipliers {
    /// All classes at 1.0/1.0: classification then changes nothing.
    pub fn neutral() -> Self {
        Self {
            symbol: ChannelMultipliers::neutral(),
            path: ChannelMultipliers::neutral(),
            signature: ChannelMultipliers::neutral(),
        }
    }

    /// The multipliers for a class. Conceptual is pinned to 1.0/1.0
    /// unconditionally (REQ-009).
    pub fn for_class(&self, class: QueryClass) -> ChannelMultipliers {
        match class {
            QueryClass::Symbol => self.symbol,
            QueryClass::Path => self.path,
            QueryClass::Signature => self.signature,
            QueryClass::Conceptual => ChannelMultipliers::neutral(),
        }
    }

    /// The effective weight table for a class: a copy of `weights` with the
    /// `lexical` and `semantic` entries scaled by the class multipliers.
    /// Every other signal passes through untouched — structural signals are
    /// class-independent (REQ-008). Absent channels stay absent.
    pub fn apply(&self, weights: &WeightTable, class: QueryClass) -> WeightTable {
        let scaling = self.for_class(class);
        let mut effective = weights.clone();
        if let Some(lexical) = effective.weights.get_mut("lexical") {
            *lexical *= scaling.lexical;
        }
        if let Some(semantic) = effective.weights.get_mut("semantic") {
            *semantic *= scaling.semantic;
        }
        effective
    }
}

/// Which ranking path `rank_and_explain` takes.
#[derive(Debug, Clone)]
pub struct RankSettings {
    /// `false` (the default, REQ-017) wraps the legacy lexicographic sort;
    /// `true` runs the signal pipeline.
    pub use_pipeline: bool,
    /// Signal weights for the pipeline path.
    pub weights: WeightTable,
    /// Query sources for shared-context preparation (BM25 constants and the
    /// embedding provider kind).
    pub sources: ContextSources,
    /// Per-class scaling of the lexical/semantic weights (TASK-095,
    /// REQ-008). Applied only on the pipeline path; neutral by default.
    pub class_multipliers: ClassMultipliers,
    /// A caller-pinned query class bypassing detection (REQ-007). `None`
    /// means detect from the pattern.
    pub pinned_class: Option<QueryClass>,
}

impl Default for RankSettings {
    fn default() -> Self {
        Self {
            use_pipeline: false,
            weights: WeightTable::kind_dominant(),
            sources: ContextSources::default(),
            class_multipliers: ClassMultipliers::neutral(),
            pinned_class: None,
        }
    }
}

impl RankSettings {
    /// Derive settings from the loaded configuration (TASK-095): the ONE
    /// place the `[rank]` section becomes pipeline settings, so flipping
    /// the default is a one-line change. `pinned` is the caller's explicit
    /// class pin (`--query-class` / MCP `query_class`), `None` = detect.
    pub fn from_config(
        rank: &crate::config::RankConfig,
        search: &crate::config::SearchConfig,
        embedding: crate::embedding::EmbeddingProviderKind,
        pinned: Option<QueryClass>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            use_pipeline: rank.enabled,
            weights: WeightTable::from_config(&rank.weights)?,
            sources: ContextSources {
                bm25: crate::bm25::Bm25Params::from(search),
                embedding,
            },
            class_multipliers: rank.class_multipliers,
            pinned_class: pinned,
        })
    }
}

/// A ranked search with its detected (or pinned) query class recorded
/// (TASK-095, DR-038): a misclassification is diagnosable from the
/// response. `query_class` is `Some` only when the pipeline path ran.
#[derive(Debug, Clone)]
pub struct RankedSearch {
    /// Ranked, deduplicated, grouped results.
    pub groups: Vec<(ResultCategory, Vec<ScoredResult>)>,
    /// The query class the pipeline classified or the caller pinned;
    /// `None` on the legacy path, which never classifies.
    pub query_class: Option<QueryClass>,
}

/// Unified ranking entry point for search results, recording the query
/// class (TASK-095, PRD-RANK-REQ-007).
///
/// Classification happens ONCE per query here. Then either the legacy
/// lexicographic sort (wrapped as unscored `ScoredResult`s — rendering
/// reads `.classified`, so output is byte-identical to today; never
/// classifies, never multiplies) or the signal pipeline — whose effective
/// weights are the configured table with the lexical/semantic channels
/// scaled by the query class BEFORE anything else runs, so zero-weight
/// skipping and the `--why` breakdown both see the effective weights —
/// followed by the ONE shared dedup/group implementation.
pub fn rank_and_explain_classed(
    results: &[crate::search::SearchResult],
    conn: Option<&Connection>,
    pattern: &str,
    settings: &RankSettings,
) -> RankedSearch {
    let classified = crate::ranker::classify_results(results, conn);
    let ranked_and_class = if settings.use_pipeline {
        let class = settings
            .pinned_class
            .unwrap_or_else(|| classify_query(pattern));
        let effective = settings.class_multipliers.apply(&settings.weights, class);
        let scored = rerank(
            classified,
            &QueryInfo { pattern },
            conn,
            &effective,
            &settings.sources,
        );
        // Score order interleaves categories under any non-kind-only
        // weight table (group_by_category groups by adjacency); bucket
        // into tier order first so every category is emitted exactly
        // once. For kind-only positive weights this is the identity
        // permutation, so equivalence with the legacy output is exact.
        (crate::ranker::bucket_by_category(scored), Some(class))
    } else {
        let legacy = crate::ranker::rank_results(classified)
            .into_iter()
            .map(|classified| ScoredResult {
                classified,
                score: 0.0,
                contributions: Vec::new(),
            })
            .collect();
        (legacy, None)
    };
    let (ranked, query_class) = ranked_and_class;
    let deduped = crate::ranker::dedup_reexports(ranked, pattern);
    RankedSearch {
        groups: crate::ranker::group_by_category(deduped),
        query_class,
    }
}

/// The groups-only view of [`rank_and_explain_classed`]; the pre-TASK-095
/// call sites keep their signature unchanged.
pub fn rank_and_explain(
    results: &[crate::search::SearchResult],
    conn: Option<&Connection>,
    pattern: &str,
    settings: &RankSettings,
) -> Vec<(ResultCategory, Vec<ScoredResult>)> {
    rank_and_explain_classed(results, conn, pattern, settings).groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_value_exact_constants() {
        assert_eq!(kind_value(ResultCategory::Definition), 1.0);
        assert_eq!(kind_value(ResultCategory::CallSite), 0.8);
        assert_eq!(kind_value(ResultCategory::Import), 0.6);
        assert_eq!(kind_value(ResultCategory::Other), 0.4);
        assert_eq!(kind_value(ResultCategory::Comment), 0.2);
        assert_eq!(kind_value(ResultCategory::Test), 0.0);
    }

    #[test]
    fn kind_value_strictly_monotone_across_tiers() {
        let ordered = [
            ResultCategory::Definition,
            ResultCategory::CallSite,
            ResultCategory::Import,
            ResultCategory::Other,
            ResultCategory::Comment,
            ResultCategory::Test,
        ];
        for pair in ordered.windows(2) {
            assert!(
                pair[0].tier() < pair[1].tier(),
                "fixture ordering must ascend by tier"
            );
            assert!(
                kind_value(pair[0]) > kind_value(pair[1]),
                "kind_value must strictly decrease as tier rises: {:?} vs {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn registry_contains_nine_signals_in_order() {
        let registry = builtin_signals();
        let names: Vec<&str> = registry.iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec![
                "kind",
                "lexical",
                "semantic",
                "centrality",
                "prominence",
                "path_character",
                "proximity",
                "signature",
                "churn"
            ]
        );
        assert_eq!(known_signal_names(), names);
    }

    #[test]
    fn weight_table_accepts_every_builtin_signal_name() {
        let mut weights = HashMap::new();
        for name in known_signal_names() {
            weights.insert(name.to_string(), 0.5);
        }
        let table = WeightTable::from_config(&weights).unwrap();
        for name in known_signal_names() {
            assert_eq!(table.weight(name), 0.5, "{name}");
        }
    }

    #[test]
    fn new_signals_zero_weight_never_evaluated() {
        // Kind-only weights over a fully seeded connection: the union of
        // requirements collapses to none (no supplementary context SQL),
        // and the breakdown carries only kind.
        let (_dir, conn) = lexical_seeded_conn();
        let results = vec![classified("a.rs", 1, "alpha", ResultCategory::Other)];
        let weights = table(&[("kind", 1.0), ("churn", 0.0)]);

        assert_eq!(
            union_reqs(&builtin_signals(), &weights),
            ContextReqs::none()
        );

        let scored = rerank_with_signals(
            builtin_signals(),
            results,
            &QueryInfo { pattern: "alpha" },
            Some(&conn),
            &weights,
            &ContextSources::default(),
        );
        assert_eq!(scored[0].contributions.len(), 1);
        assert_eq!(scored[0].contributions[0].signal, "kind");
    }

    #[test]
    fn kind_signal_reads_candidate_category_without_context() {
        let signal = KindSignal;
        assert_eq!(signal.name(), "kind");
        assert_eq!(signal.requires(), ContextReqs::none());

        let candidate = ClassifiedResult {
            result: crate::search::SearchResult {
                file: std::path::PathBuf::from("src/a.rs"),
                line: 3,
                col: 1,
                content: "fn foo() {}".to_string(),
            },
            category: ResultCategory::Definition,
            annotation: None,
        };
        let query = QueryInfo { pattern: "foo" };
        let ctx = SharedContext::default();
        assert_eq!(signal.contribution(&query, &candidate, &ctx), 1.0);
    }

    #[test]
    fn weight_table_kind_dominant_and_absent_is_zero() {
        let table = WeightTable::kind_dominant();
        assert_eq!(table.weight("kind"), 1.0);
        assert_eq!(table.weight("missing"), 0.0);
        assert_eq!(WeightTable::default().weight("kind"), 0.0);
    }

    #[test]
    fn weight_table_rejects_unknown_name_with_valid_names() {
        let mut weights = HashMap::new();
        weights.insert("nosuch_signal".to_string(), 1.0);
        let err = WeightTable::from_config(&weights).unwrap_err().to_string();
        assert!(
            err.contains("unknown signal name 'nosuch_signal'"),
            "error names the offender: {err}"
        );
        assert!(
            err.contains(
                "known: kind, lexical, semantic, centrality, prominence, path_character, \
proximity, signature, churn",
            ),
            "error lists every valid name: {err}"
        );
    }

    #[test]
    fn weight_table_rejects_non_finite_values() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut weights = HashMap::new();
            weights.insert("kind".to_string(), bad);
            let err = WeightTable::from_config(&weights).unwrap_err().to_string();
            assert!(
                err.contains("non-finite weight"),
                "NaN/inf must be rejected: {err}"
            );
        }
    }

    #[test]
    fn weight_table_accepts_any_finite_sign() {
        let mut weights = HashMap::new();
        weights.insert("kind".to_string(), -2.5);
        let table = WeightTable::from_config(&weights).unwrap();
        assert_eq!(table.weight("kind"), -2.5);
    }

    // -------------------------------------------------------------------
    // Pipeline tests
    // -------------------------------------------------------------------

    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use crate::search::SearchResult;

    fn classified(file: &str, line: u64, content: &str, cat: ResultCategory) -> ClassifiedResult {
        ClassifiedResult {
            result: SearchResult {
                file: PathBuf::from(file),
                line,
                col: 1,
                content: content.to_string(),
            },
            category: cat,
            annotation: None,
        }
    }

    /// A signal returning a fixed contribution.
    struct ConstantSignal(&'static str, f32);

    impl Signal for ConstantSignal {
        fn name(&self) -> &'static str {
            self.0
        }
        fn requires(&self) -> ContextReqs {
            ContextReqs::none()
        }
        fn contribution(
            &self,
            _query: &QueryInfo<'_>,
            _candidate: &ClassifiedResult,
            _ctx: &SharedContext,
        ) -> f32 {
            self.1
        }
    }

    /// A signal whose evaluation would be observable (and fatal).
    struct PanickySignal;

    impl Signal for PanickySignal {
        fn name(&self) -> &'static str {
            "panicky"
        }
        fn requires(&self) -> ContextReqs {
            ContextReqs::none()
        }
        fn contribution(
            &self,
            _query: &QueryInfo<'_>,
            _candidate: &ClassifiedResult,
            _ctx: &SharedContext,
        ) -> f32 {
            panic!("zero-weight signal must never be evaluated");
        }
    }

    /// A signal counting its evaluations and requiring the DB context.
    struct CountingSignal(Arc<AtomicUsize>);

    impl Signal for CountingSignal {
        fn name(&self) -> &'static str {
            "counting"
        }
        fn requires(&self) -> ContextReqs {
            ContextReqs::none().with_symbol_hits()
        }
        fn contribution(
            &self,
            _query: &QueryInfo<'_>,
            _candidate: &ClassifiedResult,
            _ctx: &SharedContext,
        ) -> f32 {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
            0.5
        }
    }

    fn table(entries: &[(&str, f32)]) -> WeightTable {
        WeightTable::from_pairs(entries.iter().map(|(n, w)| (n.to_string(), *w))).unwrap()
    }

    #[test]
    fn rerank_weighted_sum_retains_unweighted_contributions() {
        let signals: Vec<Box<dyn Signal>> = vec![
            Box::new(ConstantSignal("alpha", 0.5)),
            Box::new(ConstantSignal("beta", 0.25)),
        ];
        let results = vec![classified("src/a.rs", 1, "x", ResultCategory::Other)];
        let query = QueryInfo { pattern: "x" };
        let weights = table(&[("alpha", 2.0), ("beta", -1.0)]);

        let scored = rerank_with_signals(
            signals,
            results,
            &query,
            None,
            &weights,
            &ContextSources::default(),
        );
        assert_eq!(scored.len(), 1);
        assert_eq!(scored[0].score, 0.5 * 2.0 - 0.25);
        // Breakdown in registry order with UNWEIGHTED values retained.
        assert_eq!(scored[0].contributions.len(), 2);
        assert_eq!(
            scored[0].contributions[0],
            Contribution {
                signal: "alpha",
                value: 0.5,
                weight: 2.0,
                weighted: 1.0
            }
        );
        assert_eq!(
            scored[0].contributions[1],
            Contribution {
                signal: "beta",
                value: 0.25,
                weight: -1.0,
                weighted: -0.25
            }
        );
    }

    #[test]
    fn zero_weight_signal_is_never_evaluated() {
        let count = Arc::new(AtomicUsize::new(0));
        let signals: Vec<Box<dyn Signal>> = vec![
            Box::new(PanickySignal),
            Box::new(CountingSignal(Arc::clone(&count))),
            Box::new(KindSignal),
        ];
        let results = vec![classified("src/a.rs", 1, "x", ResultCategory::Other)];
        let query = QueryInfo { pattern: "x" };
        let weights = table(&[("kind", 1.0)]);

        let scored = rerank_with_signals(
            signals,
            results,
            &query,
            None,
            &weights,
            &ContextSources::default(),
        );
        assert_eq!(count.load(AtomicOrdering::SeqCst), 0);
        // Only the active signal appears in the breakdown.
        assert_eq!(scored[0].contributions.len(), 1);
        assert_eq!(scored[0].contributions[0].signal, "kind");
    }

    #[test]
    fn union_reqs_excludes_zero_weight_signals() {
        let count = Arc::new(AtomicUsize::new(0));
        let signals: Vec<Box<dyn Signal>> =
            vec![Box::new(CountingSignal(count)), Box::new(KindSignal)];

        let off = union_reqs(&signals, &table(&[("kind", 1.0)]));
        assert_eq!(off, ContextReqs::none());

        let on = union_reqs(&signals, &table(&[("kind", 1.0), ("counting", 0.5)]));
        assert_eq!(on, ContextReqs::none().with_symbol_hits());
    }

    // -- churn signal (TASK-096) ---------------------------------------------

    fn churn_seeded_conn() -> (tempfile::TempDir, Connection) {
        let (dir, conn) = seeded_conn();
        for (file, score) in [("src/a.rs", 4.0), ("src/b.rs", 1.0), ("src/other.rs", 99.0)] {
            conn.execute(
                "INSERT INTO file_churn (file, score) VALUES (?1, ?2)",
                rusqlite::params![file, score],
            )
            .unwrap();
        }
        (dir, conn)
    }

    #[test]
    fn churn_value_log_damped_monotone_and_zero_paths() {
        // Monotone in the score, and the set max maps to exactly 1.0.
        assert!(churn_value(5.0, 100.0) > churn_value(1.0, 100.0));
        assert_eq!(churn_value(100.0, 100.0), 1.0);
        // Log damping keeps small scores alive against a hot max.
        assert!(churn_value(5.0, 500.0) > 0.25);
        // An all-dormant set is inert, and absent is exactly zero.
        assert_eq!(churn_value(0.0, 0.0), 0.0);
        assert_eq!(churn_value(1.0, 0.0), 0.0);
        assert_eq!(churn_value(0.0, 4.0), 0.0);
        // Non-finite inputs never propagate NaN.
        assert_eq!(churn_value(f32::NAN, 4.0), 0.0);
        assert_eq!(churn_value(2.0, f32::INFINITY), 0.0);
    }

    #[test]
    fn churn_absent_from_default_weights_but_configurable() {
        // Absent = weight 0: rankings are unchanged until a user opts in.
        let defaults =
            WeightTable::from_config(&crate::config::RankConfig::default().weights).unwrap();
        assert_eq!(defaults.weight("churn"), 0.0);

        // Registered: the name is accepted by [rank.weights].
        let mut weights = HashMap::new();
        weights.insert("churn".to_string(), 1.0);
        let table = WeightTable::from_config(&weights).unwrap();
        assert_eq!(table.weight("churn"), 1.0);
    }

    #[test]
    fn churn_requires_its_context_slice() {
        assert_eq!(
            ChurnSignal.requires(),
            ContextReqs::none().with_file_churn()
        );
        assert_eq!(ChurnSignal.name(), "churn");
    }

    #[test]
    fn churn_context_loads_only_candidate_files_with_folded_max() {
        let (_dir, conn) = churn_seeded_conn();
        let results = vec![
            classified("src/a.rs", 1, "x", ResultCategory::Other),
            classified("src/b.rs", 1, "x", ResultCategory::Other),
        ];

        let ctx = prepare_context(
            ContextReqs::none().with_file_churn(),
            "x",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        assert_eq!(ctx.churn_score("src/a.rs"), Some(4.0));
        assert_eq!(ctx.churn_score("src/b.rs"), Some(1.0));
        assert_eq!(ctx.churn_score("src/other.rs"), None, "not a candidate");
        assert_eq!(ctx.max_churn(), 4.0, "max folds over the candidate set");
    }

    #[test]
    fn churn_context_empty_without_table_or_connection() {
        // A pre-TASK-096 index has no file_churn table: the presence probe
        // degrades to an empty slice, never an error.
        let raw = Connection::open_in_memory().unwrap();
        let results = vec![classified("src/a.rs", 1, "x", ResultCategory::Other)];
        let ctx = prepare_context(
            ContextReqs::none().with_file_churn(),
            "x",
            &results,
            Some(&raw),
            &ContextSources::default(),
        );
        assert_eq!(ctx.churn_score("src/a.rs"), None);
        assert_eq!(ctx.max_churn(), 0.0);

        // No connection at all: same zero-path.
        let ctx = prepare_context(
            ContextReqs::none().with_file_churn(),
            "x",
            &results,
            None,
            &ContextSources::default(),
        );
        assert_eq!(ctx.churn_score("src/a.rs"), None);
    }

    #[test]
    fn churn_signal_scores_missing_file_exactly_zero() {
        let (_dir, conn) = churn_seeded_conn();
        let results = vec![
            classified("src/a.rs", 1, "x", ResultCategory::Other),
            classified("src/gone.rs", 1, "x", ResultCategory::Other),
        ];
        let scored = rerank(
            results,
            &QueryInfo { pattern: "x" },
            Some(&conn),
            &table(&[("churn", 1.0)]),
            &ContextSources::default(),
        );
        let by_file = |f: &str| {
            scored
                .iter()
                .find(|s| s.classified.result.file == *f)
                .unwrap()
        };
        let hot = by_file("src/a.rs");
        let gone = by_file("src/gone.rs");
        assert_eq!(hot.contributions[0].signal, "churn");
        assert_eq!(hot.contributions[0].value, 1.0, "set max maps to 1.0");
        assert_eq!(
            gone.contributions[0].value, 0.0,
            "absent is zero, not a penalty"
        );
        assert!(hot.score > gone.score);
    }

    // -- co-change signal (TASK-097) ------------------------------------------

    /// handler and serializer change together (3.0); handler and migration
    /// change together more weakly (1.0); handler's coupling to other.rs
    /// (99.0) is stored but other.rs is never a candidate in these tests.
    fn co_change_seeded_conn() -> (tempfile::TempDir, Connection) {
        let (dir, conn) = seeded_conn();
        for (a, b, w) in [
            ("src/handler.rs", "src/serializer.rs", 3.0f32),
            ("src/serializer.rs", "src/handler.rs", 3.0),
            ("src/handler.rs", "src/migration.rs", 1.0),
            ("src/migration.rs", "src/handler.rs", 1.0),
            ("src/handler.rs", "src/other.rs", 99.0),
        ] {
            conn.execute(
                "INSERT INTO co_change (file_a, file_b, weight) VALUES (?1, ?2, ?3)",
                rusqlite::params![a, b, w],
            )
            .unwrap();
        }
        (dir, conn)
    }

    fn co_change_candidates() -> Vec<ClassifiedResult> {
        vec![
            classified("src/handler.rs", 1, "x", ResultCategory::Other),
            classified("src/serializer.rs", 1, "x", ResultCategory::Other),
            classified("src/migration.rs", 1, "x", ResultCategory::Other),
            classified("src/loner.rs", 1, "x", ResultCategory::Other),
        ]
    }

    #[test]
    fn co_change_value_log_damped_monotone_and_zero_paths() {
        // Monotone in the weight, and the set max maps to exactly 1.0.
        assert!(co_change_value(5.0, 100.0) > co_change_value(1.0, 100.0));
        assert_eq!(co_change_value(100.0, 100.0), 1.0);
        // Log damping keeps weak coupling alive against a strong max.
        assert!(co_change_value(5.0, 500.0) > 0.25);
        // An uncoupled set is inert, and absent is exactly zero.
        assert_eq!(co_change_value(0.0, 0.0), 0.0);
        assert_eq!(co_change_value(1.0, 0.0), 0.0);
        assert_eq!(co_change_value(0.0, 4.0), 0.0);
        // Non-finite inputs never propagate NaN.
        assert_eq!(co_change_value(f32::NAN, 4.0), 0.0);
        assert_eq!(co_change_value(2.0, f32::INFINITY), 0.0);
    }

    #[test]
    fn co_change_context_loads_only_candidate_pairs_with_folded_max() {
        let (_dir, conn) = co_change_seeded_conn();

        let ctx = prepare_context(
            ContextReqs::none().with_co_change(),
            "x",
            &co_change_candidates(),
            Some(&conn),
            &ContextSources::default(),
        );

        // Each candidate's STRONGEST coupling to another candidate: the
        // 99.0 row pairs handler with a non-candidate and must be ignored.
        assert_eq!(ctx.co_change_coupling("src/handler.rs"), Some(3.0));
        assert_eq!(ctx.co_change_coupling("src/serializer.rs"), Some(3.0));
        assert_eq!(ctx.co_change_coupling("src/migration.rs"), Some(1.0));
        assert_eq!(
            ctx.co_change_coupling("src/loner.rs"),
            None,
            "no retained coupling at all"
        );
        assert_eq!(
            ctx.co_change_coupling("src/other.rs"),
            None,
            "not a candidate"
        );
        assert_eq!(ctx.max_co_change(), 3.0, "max folds over candidate pairs");
    }

    #[test]
    fn co_change_context_empty_without_table_or_connection() {
        // A pre-TASK-097 index has no co_change table: the presence probe
        // degrades to an empty slice, never an error.
        let raw = Connection::open_in_memory().unwrap();
        let results = vec![classified("src/a.rs", 1, "x", ResultCategory::Other)];
        let ctx = prepare_context(
            ContextReqs::none().with_co_change(),
            "x",
            &results,
            Some(&raw),
            &ContextSources::default(),
        );
        assert_eq!(ctx.co_change_coupling("src/a.rs"), None);
        assert_eq!(ctx.max_co_change(), 0.0);

        // No connection at all: same zero-path.
        let ctx = prepare_context(
            ContextReqs::none().with_co_change(),
            "x",
            &results,
            None,
            &ContextSources::default(),
        );
        assert_eq!(ctx.co_change_coupling("src/a.rs"), None);
    }

    #[test]
    fn rerank_kind_only_reproduces_legacy_order() {
        let results = vec![
            classified("tests/t.rs", 9, "foo();", ResultCategory::Test),
            classified("src/b.rs", 7, "use foo;", ResultCategory::Import),
            classified("src/a.rs", 30, "// foo", ResultCategory::Comment),
            classified("src/a.rs", 3, "fn foo() {}", ResultCategory::Definition),
            classified("src/a.rs", 12, "foo();", ResultCategory::CallSite),
            classified("src/z.rs", 1, "let x = foo;", ResultCategory::Other),
            classified("src/a.rs", 12, "foo();", ResultCategory::CallSite),
        ];
        let legacy = crate::ranker::rank_results(results.clone());

        let scored = rerank(
            results,
            &QueryInfo { pattern: "foo" },
            None,
            &WeightTable::kind_dominant(),
            &ContextSources::default(),
        );

        let key = |c: &ClassifiedResult| (c.result.file.clone(), c.result.line);
        let expected: Vec<_> = legacy.iter().map(key).collect();
        let actual: Vec<_> = scored.iter().map(|s| key(&s.classified)).collect();
        assert_eq!(actual, expected);
        // Every result is scored kind*1.0 with its tier constant.
        for s in &scored {
            assert_eq!(s.score, kind_value(s.classified.category));
        }
    }

    #[test]
    fn rerank_is_pure_across_input_orders() {
        let make = || {
            vec![
                classified("src/b.rs", 7, "use foo;", ResultCategory::Import),
                classified("src/a.rs", 3, "fn foo() {}", ResultCategory::Definition),
                classified("src/a.rs", 12, "foo();", ResultCategory::CallSite),
                classified("tests/t.rs", 9, "foo();", ResultCategory::Test),
                classified("src/z.rs", 1, "let x = foo;", ResultCategory::Other),
            ]
        };
        let mut swapped = make();
        swapped.reverse();
        let query = QueryInfo { pattern: "foo" };
        let a = rerank(
            make(),
            &query,
            None,
            &WeightTable::kind_dominant(),
            &ContextSources::default(),
        );
        let b = rerank(
            swapped,
            &query,
            None,
            &WeightTable::kind_dominant(),
            &ContextSources::default(),
        );
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.classified, y.classified);
            assert_eq!(x.score, y.score);
        }
    }

    #[test]
    fn compare_scored_orders_score_desc_then_file_then_line() {
        let mk = |file: &str, line: u64, score: f32| ScoredResult {
            classified: classified(file, line, "x", ResultCategory::Other),
            score,
            contributions: vec![],
        };
        let hi = mk("src/z.rs", 99, 1.0);
        let lo = mk("src/a.rs", 1, 0.2);
        assert_eq!(compare_scored(&hi, &lo), std::cmp::Ordering::Less);

        let same_a1 = mk("src/a.rs", 1, 0.5);
        let same_a2 = mk("src/a.rs", 2, 0.5);
        assert_eq!(
            compare_scored(&same_a2, &same_a1),
            std::cmp::Ordering::Greater
        );

        let same_b = mk("src/b.rs", 1, 0.5);
        assert_eq!(
            compare_scored(&same_b, &same_a1),
            std::cmp::Ordering::Greater
        );

        let dup = mk("src/a.rs", 1, 0.5);
        assert_eq!(compare_scored(&same_a1, &dup), std::cmp::Ordering::Equal);
    }

    #[test]
    fn compare_scored_treats_nan_as_equal_on_score_axis() {
        let mk = |file: &str, line: u64, score: f32| ScoredResult {
            classified: classified(file, line, "x", ResultCategory::Other),
            score,
            contributions: vec![],
        };
        let nan = mk("src/a.rs", 1, f32::NAN);
        let plain = mk("src/z.rs", 9, 0.5);
        // NaN vs anything must not panic and must fall through to file.
        assert_eq!(compare_scored(&nan, &plain), std::cmp::Ordering::Less);
        let nan2 = mk("src/a.rs", 1, f32::NAN);
        assert_eq!(compare_scored(&nan, &nan2), std::cmp::Ordering::Equal);
    }

    // -------------------------------------------------------------------
    // prepare_context tests
    // -------------------------------------------------------------------

    /// Seed an index DB: `my_func` defined at src/main.rs:10, called from
    /// two distinct symbols (plus one reference with an unknown caller).
    fn seeded_conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["my_func", "function", "src/main.rs", 10, 0, "rust"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["caller_a", "function", "src/other.rs", 1, 0, "rust"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["caller_b", "function", "src/third.rs", 1, 0, "rust"],
        )
        .unwrap();
        for caller in [Some(2i64), Some(3), None] {
            conn.execute(
                "INSERT INTO \"references\" (name, file, line, col, context, caller_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params!["my_func", "src/main.rs", 11, 4, "my_func();", caller],
            )
            .unwrap();
        }
        (dir, conn)
    }

    #[test]
    fn prepare_context_default_reqs_touches_nothing_on_seeded_db() {
        let (_dir, conn) = seeded_conn();
        let results = vec![classified(
            "src/main.rs",
            10,
            "fn my_func() {}",
            ResultCategory::Definition,
        )];

        let ctx = prepare_context(
            ContextReqs::none(),
            "my_func",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        // The DB has a matching symbol row; emptiness proves the gate
        // short-circuited before any SQL ran.
        assert!(ctx.symbol_hits.is_empty());
        assert!(ctx.path_class.is_empty());
        assert!(ctx.terms.is_empty());
    }

    #[test]
    fn prepare_context_symbol_hits_carry_caller_counts() {
        let (_dir, conn) = seeded_conn();
        let results = vec![classified(
            "src/main.rs",
            10,
            "fn my_func() {}",
            ResultCategory::Definition,
        )];

        let ctx = prepare_context(
            ContextReqs::none().with_symbol_hits(),
            "my_func",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        let hit = ctx
            .symbol_hit("src/main.rs", 10)
            .expect("symbol at candidate position");
        assert_eq!(hit.name, "my_func");
        assert_eq!(hit.kind, "function");
        // Two distinct caller_ids; the NULL caller row is not counted.
        assert_eq!(hit.caller_count, 2);
        // A non-candidate line has no hit even though the file is loaded.
        assert!(ctx.symbol_hit("src/main.rs", 11).is_none());
    }

    #[test]
    fn prepare_context_without_conn_leaves_symbol_hits_empty() {
        let results = vec![classified(
            "src/main.rs",
            10,
            "fn my_func() {}",
            ResultCategory::Definition,
        )];
        let ctx = prepare_context(
            ContextReqs::none().with_symbol_hits(),
            "my_func",
            &results,
            None,
            &ContextSources::default(),
        );
        assert!(ctx.symbol_hits.is_empty());
    }

    #[test]
    fn prepare_context_classifies_each_unique_file_once() {
        let results = vec![
            classified("src/a.rs", 1, "x", ResultCategory::Other),
            classified("src/a.rs", 5, "x", ResultCategory::Other),
            classified("tests/b.rs", 1, "x", ResultCategory::Other),
        ];
        let ctx = prepare_context(
            ContextReqs::none().with_path_class(),
            "x",
            &results,
            None,
            &ContextSources::default(),
        );
        assert_eq!(ctx.path_class.len(), 2);
        assert_eq!(ctx.path_class("src/a.rs"), Some(PathClass::Ordinary));
        assert_eq!(ctx.path_class("tests/b.rs"), Some(PathClass::Test));
    }

    #[test]
    fn prepare_context_tokenizes_the_query_once() {
        let ctx = prepare_context(
            ContextReqs::none().with_query_terms(),
            "Cache Eviction!",
            &[],
            None,
            &ContextSources::default(),
        );
        assert_eq!(ctx.terms(), &["cache".to_string(), "eviction".to_string()]);
    }

    #[test]
    fn rank_and_explain_emits_each_category_once_under_interleaved_scores() {
        // kind = 0.0 is a documented, valid config ("A weight of 0 skips
        // the signal entirely", docs/configuration.md): the kind signal is
        // never evaluated, every score is 0.0, and pipeline ordering falls
        // to the (file, line) tie-breaks — interleaving categories. The
        // grouped output must still emit each category EXACTLY once (tier
        // order), never one group per adjacent run.
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        for file in ["src/a.rs", "src/c.rs"] {
            conn.execute(
                "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params!["my_func", "function", file, 1, 0, "rust"],
            )
            .unwrap();
        }
        for file in ["src/b.rs", "src/d.rs"] {
            conn.execute(
                "INSERT INTO \"references\" (name, file, line, col, context) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["my_func", file, 1, 4, "my_func();"],
            )
            .unwrap();
        }
        let mk = |file: &str, content: &str| SearchResult {
            file: PathBuf::from(file),
            line: 1,
            col: 1,
            content: content.to_string(),
        };
        let results = vec![
            mk("src/a.rs", "pub fn my_func() {}"), // Definition (index)
            mk("src/b.rs", "my_func();"),          // CallSite (index)
            mk("src/c.rs", "pub fn my_func() {}"), // Definition (index)
            mk("src/d.rs", "my_func();"),          // CallSite (index)
            mk("src/e.rs", "// my_func note"),     // Comment (content)
        ];
        let settings = RankSettings {
            use_pipeline: true,
            weights: table(&[("kind", 0.0)]),
            ..Default::default()
        };

        let groups = rank_and_explain(&results, Some(&conn), "my_func", &settings);

        let cats: Vec<ResultCategory> = groups.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            cats,
            vec![
                ResultCategory::Definition,
                ResultCategory::CallSite,
                ResultCategory::Comment,
            ],
            "interleaved score order must not fragment category groups: {cats:?}"
        );
        // Within each bucket the score tie-break ((file, line)) order is
        // preserved, not re-sorted.
        let files = |items: &Vec<ScoredResult>| -> Vec<String> {
            items
                .iter()
                .map(|s| s.classified.result.file.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(files(&groups[0].1), vec!["src/a.rs", "src/c.rs"]);
        assert_eq!(files(&groups[1].1), vec!["src/b.rs", "src/d.rs"]);
    }

    // -------------------------------------------------------------------
    // TASK-093 context plumbing
    // -------------------------------------------------------------------

    #[test]
    fn context_sources_default_matches_search_defaults() {
        let sources = ContextSources::default();
        assert_eq!(
            sources.bm25,
            crate::bm25::Bm25Params::from(&crate::config::SearchConfig::default())
        );
        assert_eq!(
            sources.embedding,
            crate::embedding::EmbeddingProviderKind::Bundled
        );
    }

    /// Seed term_stats/files rows so `file_bm25_scores` returns real values:
    /// a.rs carries tf 5 for "alpha", b.rs tf 1, both 100 lines.
    fn lexical_seeded_conn() -> (tempfile::TempDir, Connection) {
        let (dir, conn) = seeded_conn();
        for path in ["a.rs", "b.rs"] {
            conn.execute(
                "INSERT INTO files (path, language, hash, last_indexed, line_count) \
                 VALUES (?1, 'rust', 'h', 0, 100)",
                rusqlite::params![path],
            )
            .unwrap();
        }
        for (file, tf) in [("a.rs", 5), ("b.rs", 1)] {
            conn.execute(
                "INSERT INTO term_stats (term, file, tf) VALUES (?1, ?2, ?3)",
                rusqlite::params!["alpha", file, tf],
            )
            .unwrap();
        }
        (dir, conn)
    }

    #[test]
    fn prepare_context_lexical_gated_off_leaves_scores_empty() {
        let (_dir, conn) = lexical_seeded_conn();
        let results = vec![classified("a.rs", 1, "alpha", ResultCategory::Other)];

        let ctx = prepare_context(
            ContextReqs::none().with_symbol_hits(),
            "alpha",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        assert_eq!(ctx.lexical_score("a.rs"), None);
        assert_eq!(ctx.lexical_bounds(), None);
    }

    #[test]
    fn prepare_context_lexical_scores_and_bounds_folded_once() {
        let (_dir, conn) = lexical_seeded_conn();
        let results = vec![
            classified("a.rs", 1, "alpha", ResultCategory::Other),
            classified("b.rs", 1, "alpha", ResultCategory::Other),
        ];

        let ctx = prepare_context(
            ContextReqs::none().with_lexical_scores(),
            "alpha",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        let (min, max) = ctx.lexical_bounds().expect("bounds folded with the scores");
        let score_a = ctx.lexical_score("a.rs").expect("a.rs scored");
        let score_b = ctx.lexical_score("b.rs").expect("b.rs scored");
        assert!(
            score_a > score_b,
            "tf 5 must beat tf 1: {score_a} vs {score_b}"
        );
        assert_eq!(min, score_b);
        assert_eq!(max, score_a);
    }

    // -------------------------------------------------------------------
    // LexicalSignal
    // -------------------------------------------------------------------

    #[test]
    fn min_max_normalize_exact_and_clamped() {
        assert!((min_max_normalize(0.5, 0.0, 1.0) - 0.5).abs() < 1e-6);
        assert_eq!(min_max_normalize(0.0, 0.0, 1.0), 0.0);
        assert_eq!(min_max_normalize(1.0, 0.0, 1.0), 1.0);
        // Out-of-range inputs clamp instead of amplifying.
        assert_eq!(min_max_normalize(-1.0, 0.0, 1.0), 0.0);
        assert_eq!(min_max_normalize(2.0, 0.0, 1.0), 1.0);
        assert_eq!(min_max_normalize(3.0, 1.0, 2.0), 1.0);
    }

    #[test]
    fn min_max_normalize_degenerate_range_and_nan_are_zero() {
        // An all-equal set (max == min) has no contrast: everyone gets 0.
        assert_eq!(min_max_normalize(3.0, 3.0, 3.0), 0.0);
        // An inverted range (max < min) is degenerate, not an error.
        assert_eq!(min_max_normalize(0.0, 2.0, 1.0), 0.0);
        // NaN and non-finite bounds normalize to 0, never propagate.
        assert_eq!(min_max_normalize(f32::NAN, 0.0, 1.0), 0.0);
        assert_eq!(
            min_max_normalize(0.5, f32::NEG_INFINITY, f32::INFINITY),
            0.0
        );
    }

    #[test]
    fn lexical_signal_inherits_file_score_and_set_normalizes() {
        let (_dir, conn) = lexical_seeded_conn();
        let results = vec![
            classified("a.rs", 1, "alpha", ResultCategory::Other),
            classified("a.rs", 7, "alpha", ResultCategory::Other),
            classified("b.rs", 3, "alpha", ResultCategory::Other),
        ];

        let ctx = prepare_context(
            ContextReqs::none().with_lexical_scores(),
            "alpha",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        let signal = LexicalSignal;
        assert_eq!(signal.name(), "lexical");
        assert_eq!(signal.requires(), ContextReqs::none().with_lexical_scores());
        let query = QueryInfo { pattern: "alpha" };
        // a.rs holds the set max (tf 5), b.rs the min (tf 1); lines in the
        // same file share the file's score.
        assert_eq!(signal.contribution(&query, &results[0], &ctx), 1.0);
        assert_eq!(signal.contribution(&query, &results[1], &ctx), 1.0);
        assert_eq!(signal.contribution(&query, &results[2], &ctx), 0.0);
    }

    #[test]
    fn lexical_signal_zero_when_context_missing() {
        // No term_stats rows in seeded_conn: scoring is unavailable and the
        // signal contributes exactly zero rather than erroring.
        let (_dir, conn) = seeded_conn();
        let results = vec![classified(
            "src/main.rs",
            10,
            "fn my_func() {}",
            ResultCategory::Definition,
        )];
        let ctx = prepare_context(
            ContextReqs::none().with_lexical_scores(),
            "my_func",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );
        let signal = LexicalSignal;
        let query = QueryInfo { pattern: "my_func" };
        assert_eq!(signal.contribution(&query, &results[0], &ctx), 0.0);

        // Gated-off preparation leaves the same zero contribution.
        let gated = prepare_context(
            ContextReqs::none(),
            "my_func",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );
        assert_eq!(signal.contribution(&query, &results[0], &gated), 0.0);
    }

    #[test]
    fn lexical_signal_zero_when_set_is_uniform() {
        let (_dir, conn) = lexical_seeded_conn();
        // Same tf for both files: the raw scores tie, the range is zero, and
        // every member contributes 0 (no contrast to amplify).
        conn.execute("UPDATE term_stats SET tf = 3", []).unwrap();
        let results = vec![
            classified("a.rs", 1, "alpha", ResultCategory::Other),
            classified("b.rs", 1, "alpha", ResultCategory::Other),
        ];
        let ctx = prepare_context(
            ContextReqs::none().with_lexical_scores(),
            "alpha",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );
        let signal = LexicalSignal;
        let query = QueryInfo { pattern: "alpha" };
        assert_eq!(signal.contribution(&query, &results[0], &ctx), 0.0);
        assert_eq!(signal.contribution(&query, &results[1], &ctx), 0.0);
    }

    #[test]
    fn prepare_context_default_reqs_leaves_new_slices_empty() {
        let (_dir, conn) = lexical_seeded_conn();
        let results = vec![classified("a.rs", 1, "alpha", ResultCategory::Other)];

        let ctx = prepare_context(
            ContextReqs::none(),
            "alpha",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        assert_eq!(ctx.lexical_bounds(), None);
        assert_eq!(ctx.max_caller_count(), 0);
        assert!(ctx.query_embedding().is_none());
        assert!(ctx.embedding_at("a.rs", 1).is_none());
    }

    #[test]
    fn prepare_context_folds_max_caller_count_once() {
        let (_dir, conn) = seeded_conn();
        let results = vec![classified(
            "src/main.rs",
            10,
            "fn my_func() {}",
            ResultCategory::Definition,
        )];

        let ctx = prepare_context(
            ContextReqs::none().with_symbol_hits(),
            "my_func",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        assert_eq!(ctx.max_caller_count(), 2);
    }

    // -------------------------------------------------------------------
    // CentralitySignal
    // -------------------------------------------------------------------

    #[test]
    fn centrality_value_exact_log_formula() {
        let expected = (1.0f32 + 5.0).ln() / (1.0f32 + 500.0).ln();
        assert!((centrality_value(5, 500) - expected).abs() < 1e-4);
        // The hub itself saturates at exactly 1.0.
        assert_eq!(centrality_value(500, 500), 1.0);
        assert_eq!(centrality_value(2, 2), 1.0);
    }

    #[test]
    fn centrality_value_zero_floor_and_all_zero_set() {
        // No callers: a floor, not a penalty.
        assert_eq!(centrality_value(0, 500), 0.0);
        // An all-zero set has no denominator: everything is 0.
        assert_eq!(centrality_value(0, 0), 0.0);
        assert_eq!(centrality_value(3, 0), 0.0);
    }

    #[test]
    fn centrality_value_monotone_and_log_damped() {
        let mut prev = centrality_value(0, 500);
        for count in [1u32, 5, 20, 100, 500] {
            let value = centrality_value(count, 500);
            assert!(value > prev, "more callers must score higher");
            prev = value;
        }
        // Log damping is the point: 5-of-500 callers retains ~0.288 of the
        // range where a linear ratio keeps 1% — a mid-tier symbol stays
        // competitive with the hub instead of being crushed to zero.
        let log_ratio = centrality_value(5, 500);
        let linear_ratio = 5.0f32 / 500.0f32;
        assert!(
            log_ratio > 0.28,
            "log ratio must stay substantial: {log_ratio}"
        );
        assert!((linear_ratio - 0.01).abs() < 1e-6);
        assert!(log_ratio > linear_ratio * 20.0);
    }

    #[test]
    fn centrality_signal_reads_hit_and_set_max_from_prepared_context() {
        let (_dir, conn) = seeded_conn();
        let results = vec![
            classified(
                "src/main.rs",
                10,
                "fn my_func() {}",
                ResultCategory::Definition,
            ),
            classified("src/main.rs", 11, "my_func();", ResultCategory::CallSite),
        ];

        let ctx = prepare_context(
            ContextReqs::none().with_symbol_hits(),
            "my_func",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        let signal = CentralitySignal;
        assert_eq!(signal.name(), "centrality");
        assert_eq!(signal.requires(), ContextReqs::none().with_symbol_hits());
        let query = QueryInfo { pattern: "my_func" };
        // The definition carries caller_count 2, the set max: ln(3)/ln(3).
        assert_eq!(signal.contribution(&query, &results[0], &ctx), 1.0);
        // A candidate with no symbol hit (a call site line) is genuinely
        // un-called: exactly 0.
        assert_eq!(signal.contribution(&query, &results[1], &ctx), 0.0);
    }

    // -------------------------------------------------------------------
    // ProminenceSignal
    // -------------------------------------------------------------------

    #[test]
    fn contains_identifier_token_matches_maximal_runs_only() {
        assert!(contains_identifier_token("map(x, y);", "map"));
        assert!(
            contains_identifier_token("let MAP = 3;", "map"),
            "case-folded"
        );
        assert!(
            contains_identifier_token("a map:", "map"),
            "run ending at :"
        );
        assert!(
            contains_identifier_token("map", "map"),
            "whole line is the run"
        );
        // A term inside a LONGER identifier is a different name entirely.
        assert!(!contains_identifier_token("my_map_value();", "map"));
        assert!(!contains_identifier_token("foo_bar", "foo"));
        assert!(!contains_identifier_token("foo_bar", "bar"));
        assert!(!contains_identifier_token("remapped!", "map"));
        assert!(!contains_identifier_token("", "map"));
        assert!(!contains_identifier_token("anything", ""));
    }

    #[test]
    fn prominence_signal_three_tiers() {
        // gamma defined at src/def.rs:5 (hit name "gamma"), called at
        // src/call.rs:9 (no hit), mentioned incidentally at src/note.rs:2
        // (substring only).
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["gamma", "function", "src/def.rs", 5, 0, "rust"],
        )
        .unwrap();
        let results = vec![
            classified("src/def.rs", 5, "fn gamma() {}", ResultCategory::Definition),
            classified("src/call.rs", 9, "gamma(3);", ResultCategory::CallSite),
            classified(
                "src/note.rs",
                2,
                "// see gamma_helper",
                ResultCategory::Comment,
            ),
        ];

        let ctx = prepare_context(
            ContextReqs::none().with_query_terms().with_symbol_hits(),
            "gamma",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        let signal = ProminenceSignal;
        assert_eq!(signal.name(), "prominence");
        assert_eq!(
            signal.requires(),
            ContextReqs::none().with_query_terms().with_symbol_hits()
        );
        let query = QueryInfo { pattern: "gamma" };
        // Tier 1: the hit's name equals the query term.
        assert_eq!(signal.contribution(&query, &results[0], &ctx), 1.0);
        // Tier 2: the term appears as a maximal identifier run.
        assert_eq!(signal.contribution(&query, &results[1], &ctx), 0.5);
        // Tier 0: substring-only inside a longer identifier.
        assert_eq!(signal.contribution(&query, &results[2], &ctx), 0.0);
    }

    #[test]
    fn prominence_signal_multi_term_takes_best_tier() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["map", "function", "src/m.rs", 1, 0, "rust"],
        )
        .unwrap();
        let results = vec![
            classified("src/m.rs", 1, "pub fn map()", ResultCategory::Definition),
            classified("src/f.rs", 1, "foo = 1;", ResultCategory::Other),
        ];

        let ctx = prepare_context(
            ContextReqs::none().with_query_terms().with_symbol_hits(),
            "map foo",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        let signal = ProminenceSignal;
        let query = QueryInfo { pattern: "map foo" };
        // "map" hits tier 1 for the definition even though "foo" only
        // token-matches elsewhere: the best tier wins.
        assert_eq!(signal.contribution(&query, &results[0], &ctx), 1.0);
        assert_eq!(signal.contribution(&query, &results[1], &ctx), 0.5);
    }

    #[test]
    fn prominence_signal_zero_without_terms() {
        let results = vec![classified(
            "src/a.rs",
            1,
            "fn foo()",
            ResultCategory::Definition,
        )];
        // Punctuation-only pattern: no terms, no tiers.
        let ctx = prepare_context(
            ContextReqs::none().with_query_terms().with_symbol_hits(),
            ":: - _",
            &results,
            None,
            &ContextSources::default(),
        );
        let signal = ProminenceSignal;
        let query = QueryInfo { pattern: ":: - _" };
        assert_eq!(signal.contribution(&query, &results[0], &ctx), 0.0);
    }

    #[test]
    fn prominence_signal_raw_pattern_matches_compound_names() {
        // Query "my_func" tokenizes to [my, func] — neither equals the
        // indexed name "my_func", and neither token-matches the maximal run
        // "my_func". Only the raw-pattern comparison can award tier 1.
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["my_func", "function", "src/main.rs", 10, 0, "rust"],
        )
        .unwrap();
        let results = vec![
            classified(
                "src/main.rs",
                10,
                "fn my_func() {}",
                ResultCategory::Definition,
            ),
            classified("src/call.rs", 3, "my_func();", ResultCategory::CallSite),
        ];

        let ctx = prepare_context(
            ContextReqs::none().with_query_terms().with_symbol_hits(),
            "my_func",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        let signal = ProminenceSignal;
        let query = QueryInfo { pattern: "my_func" };
        assert_eq!(signal.contribution(&query, &results[0], &ctx), 1.0);
        // The call line's maximal run is the compound "my_func": neither
        // term matches it, so the mention is incidental.
        assert_eq!(signal.contribution(&query, &results[1], &ctx), 0.0);
    }

    // -------------------------------------------------------------------
    // ProximitySignal (TASK-094, REQ-013)
    // -------------------------------------------------------------------

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn identifier_tokens_scans_maximal_runs_lowercased() {
        assert_eq!(
            identifier_tokens("let MAP_size = call(x);"),
            vec![
                "let".to_string(),
                "map_size".to_string(),
                "call".to_string(),
                "x".to_string()
            ]
        );
        // Separator-only and empty lines produce no runs ("_" is an
        // identifier character, so it alone would be one run).
        assert!(identifier_tokens(":: - (!)").is_empty());
        assert!(identifier_tokens("").is_empty());
    }

    #[test]
    fn proximity_value_adjacent_terms_score_one() {
        // First-occurrence token indices 0 and 1: gap 1 → 1.0.
        assert_eq!(
            proximity_value("alpha beta;", &terms(&["alpha", "beta"])),
            1.0
        );
        // Case folding matches the scanner's lowercased tokens.
        assert_eq!(
            proximity_value("Alpha(BETA)", &terms(&["alpha", "beta"])),
            1.0
        );
    }

    #[test]
    fn proximity_value_decays_hyperbolically_with_gap() {
        // gap 2 → 0.5, gap 3 → 1/3.
        assert_eq!(
            proximity_value("alpha x beta", &terms(&["alpha", "beta"])),
            0.5
        );
        let third = proximity_value("alpha x y beta", &terms(&["alpha", "beta"]));
        assert!((third - 1.0 / 3.0).abs() < 1e-6, "{third}");
    }

    #[test]
    fn proximity_value_inert_below_two_present_terms() {
        // One present term: nothing to co-locate.
        assert_eq!(
            proximity_value("alpha other", &terms(&["alpha", "beta"])),
            0.0
        );
        // None present, empty terms, empty content.
        assert_eq!(
            proximity_value("unrelated", &terms(&["alpha", "beta"])),
            0.0
        );
        assert_eq!(proximity_value("alpha beta", &terms(&[])), 0.0);
        assert_eq!(proximity_value("", &terms(&["alpha", "beta"])), 0.0);
        assert_eq!(proximity_value("", &terms(&[])), 0.0);
    }

    #[test]
    fn proximity_value_matches_whole_tokens_only() {
        // "foo" inside foo_bar names a different identifier: not present,
        // so no co-location is claimed.
        assert_eq!(proximity_value("foo_bar;", &terms(&["foo", "bar"])), 0.0);
        // Repeated QUERY terms dedup to one first occurrence: a term never
        // co-locates with itself...
        assert_eq!(proximity_value("alpha;", &terms(&["alpha", "alpha"])), 0.0);
        // ...and the gap uses FIRST occurrences in the line.
        assert_eq!(
            proximity_value("alpha alpha beta", &terms(&["alpha", "beta"])),
            0.5
        );
    }

    #[test]
    fn proximity_signal_reads_terms_from_prepared_context() {
        let results = vec![
            classified("src/a.rs", 1, "alpha beta;", ResultCategory::Other),
            classified("src/b.rs", 1, "alpha filler beta;", ResultCategory::Other),
            classified("src/c.rs", 1, "// unrelated", ResultCategory::Other),
        ];
        let ctx = prepare_context(
            ContextReqs::none().with_query_terms(),
            "alpha beta",
            &results,
            None,
            &ContextSources::default(),
        );

        let signal = ProximitySignal;
        assert_eq!(signal.name(), "proximity");
        assert_eq!(signal.requires(), ContextReqs::none().with_query_terms());
        let query = QueryInfo {
            pattern: "alpha beta",
        };
        assert_eq!(signal.contribution(&query, &results[0], &ctx), 1.0);
        assert!((signal.contribution(&query, &results[1], &ctx) - 0.5).abs() < 1e-6);
        assert_eq!(signal.contribution(&query, &results[2], &ctx), 0.0);
    }

    // -------------------------------------------------------------------
    // SignatureSignal (TASK-094, REQ-014)
    // -------------------------------------------------------------------

    #[test]
    fn is_signature_query_detects_signature_punctuation() {
        for pattern in [
            "foo(a, b)",
            "fn foo() -> Result<T>",
            "Foo::bar",
            "  foo(x)  ",
            "(x)",
        ] {
            assert!(is_signature_query(pattern), "{pattern:?}");
        }
        for pattern in ["foo", "my_func", "error handling", ""] {
            assert!(!is_signature_query(pattern), "{pattern:?}");
        }
    }

    #[test]
    fn signature_value_inert_when_query_not_shaped() {
        // A name-shaped query must not reorder anything through this
        // signal — not even an index-backed Definition.
        assert_eq!(
            signature_value(
                false,
                ResultCategory::Definition,
                "pub fn parse(input: &str) {}"
            ),
            0.0
        );
        assert_eq!(
            signature_value(false, ResultCategory::CallSite, "parse(x);"),
            0.0
        );
    }

    #[test]
    fn signature_value_definition_category_scores_one() {
        assert_eq!(
            signature_value(true, ResultCategory::Definition, "anything"),
            1.0
        );
    }

    #[test]
    fn signature_value_definition_shaped_line_scores_half() {
        for line in [
            "pub fn parse(input: &str) -> Vec<Token> {",
            "    fn helper(x: u32) {}",
            "def process(data):",
            "export function alpha(x: number) { return x; }",
            "class Client { constructor(opts) {} }",
            "struct Config(String);",
        ] {
            assert_eq!(
                signature_value(true, ResultCategory::Other, line),
                0.5,
                "{line}"
            );
        }
    }

    #[test]
    fn signature_value_call_sites_and_late_keywords_score_zero() {
        // A call line carries the parenthesis but no definition keyword in
        // its first three identifier tokens.
        assert_eq!(
            signature_value(true, ResultCategory::CallSite, "parse(data);"),
            0.0
        );
        assert_eq!(
            signature_value(true, ResultCategory::Other, "std::mem::swap(a, b);"),
            0.0
        );
        // A definition keyword BEYOND the first three tokens is not a
        // definition line.
        assert_eq!(
            signature_value(
                true,
                ResultCategory::Other,
                "// the constructor foo(x) fn later"
            ),
            0.0
        );
    }

    #[test]
    fn signature_signal_needs_no_context_and_reads_the_query_shape() {
        let signal = SignatureSignal;
        assert_eq!(signal.name(), "signature");
        assert_eq!(signal.requires(), ContextReqs::none());

        let definition = classified(
            "src/parse.rs",
            3,
            "fn parse(input: &str) {}",
            ResultCategory::Definition,
        );
        let call = classified("src/main.rs", 9, "parse(data);", ResultCategory::CallSite);
        let ctx = SharedContext::default();

        // Shaped query: definition 1.0, call site 0.0.
        let shaped = QueryInfo {
            pattern: "parse(input: &str)",
        };
        assert_eq!(signal.contribution(&shaped, &definition, &ctx), 1.0);
        assert_eq!(signal.contribution(&shaped, &call, &ctx), 0.0);

        // Same candidates, name-shaped query: inert everywhere.
        let plain = QueryInfo { pattern: "parse" };
        assert_eq!(signal.contribution(&plain, &definition, &ctx), 0.0);
        assert_eq!(signal.contribution(&plain, &call, &ctx), 0.0);
    }

    // -------------------------------------------------------------------
    // Path-character ladder (TASK-094)
    // -------------------------------------------------------------------

    #[test]
    fn path_character_value_nine_exact_constants() {
        assert_eq!(path_character_value(PathClass::GeneratedShadowed), 0.10);
        assert_eq!(path_character_value(PathClass::Generated), 0.10);
        assert_eq!(path_character_value(PathClass::Test), 0.20);
        assert_eq!(path_character_value(PathClass::TypeDeclaration), 0.30);
        assert_eq!(path_character_value(PathClass::Shim), 0.45);
        assert_eq!(path_character_value(PathClass::Example), 0.60);
        assert_eq!(path_character_value(PathClass::Barrel), 0.70);
        assert_eq!(path_character_value(PathClass::ModuleEntry), 0.80);
        assert_eq!(path_character_value(PathClass::Ordinary), 1.00);
    }

    #[test]
    fn path_character_value_strictly_positive_ladder() {
        // 0.0 means "no evidence" in this codebase (zero-weight = inert),
        // so every bucket is strictly positive; walking the ladder from the
        // strongest demotion to Ordinary the value never decreases, and
        // every distinct bucket strictly increases.
        let ladder = [
            PathClass::GeneratedShadowed,
            PathClass::Generated,
            PathClass::Test,
            PathClass::TypeDeclaration,
            PathClass::Shim,
            PathClass::Example,
            PathClass::Barrel,
            PathClass::ModuleEntry,
            PathClass::Ordinary,
        ];
        for class in ladder {
            assert!(
                path_character_value(class) > 0.0,
                "{class:?} must be strictly positive"
            );
        }
        let distinct: Vec<f32> = ladder.map(path_character_value).to_vec();
        let deduped: Vec<f32> = {
            let mut v = distinct.clone();
            v.dedup();
            v
        };
        for pair in deduped.windows(2) {
            assert!(
                pair[0] < pair[1],
                "distinct ladder values must strictly increase"
            );
        }
    }

    #[test]
    fn classify_path_character_bucket_positives() {
        let cases: &[(&str, PathClass)] = &[
            // Test: directories, *_test stems, .test./.spec. names.
            ("tests/foo.rs", PathClass::Test),
            ("test/foo.js", PathClass::Test),
            ("__tests__/foo.js", PathClass::Test),
            ("src/foo_test.go", PathClass::Test),
            ("src/foo.test.ts", PathClass::Test),
            ("src/foo.spec.js", PathClass::Test),
            // TypeDeclaration: ambient declaration and C header extensions.
            ("src/foo.d.ts", PathClass::TypeDeclaration),
            ("types/foo.d.mts", PathClass::TypeDeclaration),
            ("src/foo.d.cts", PathClass::TypeDeclaration),
            ("include/foo.h", PathClass::TypeDeclaration),
            // Shim: compatibility directories and stems.
            ("compat/foo.ts", PathClass::Shim),
            ("src/compatibility/foo.js", PathClass::Shim),
            ("shims/polyfill.js", PathClass::Shim),
            ("deprecated/a.js", PathClass::Shim),
            ("src/legacy/b.ts", PathClass::Shim),
            ("src/compat.ts", PathClass::Shim),
            ("src/shim.js", PathClass::Shim),
            ("src/deprecated.rs", PathClass::Shim),
            ("src/foo_compat.ts", PathClass::Shim),
            ("src/foo_shim.js", PathClass::Shim),
            // Example: documentation and sample directories, .example. names.
            ("example/a.ts", PathClass::Example),
            ("examples/b.js", PathClass::Example),
            ("samples/c.py", PathClass::Example),
            ("demos/d.rs", PathClass::Example),
            ("fixtures/e.json", PathClass::Example),
            ("bench/f.ts", PathClass::Example),
            ("benchmarks/g.js", PathClass::Example),
            ("docs/h.md", PathClass::Example),
            ("doc/i.txt", PathClass::Example),
            ("src/foo.example.ts", PathClass::Example),
            // Barrel: re-export entry points.
            ("src/index.ts", PathClass::Barrel),
            ("src/index.tsx", PathClass::Barrel),
            ("web/index.js", PathClass::Barrel),
            ("src/index.jsx", PathClass::Barrel),
            ("src/index.mjs", PathClass::Barrel),
            ("src/index.cjs", PathClass::Barrel),
            ("src/mod.rs", PathClass::Barrel),
            ("src/lib.rs", PathClass::Barrel),
            ("pkg/__init__.py", PathClass::Barrel),
            ("src/exports.ts", PathClass::Barrel),
            ("src/exports.js", PathClass::Barrel),
            // ModuleEntry: program entry points.
            ("src/main.rs", PathClass::ModuleEntry),
            ("cmd/app/main.go", PathClass::ModuleEntry),
            ("scripts/main.py", PathClass::ModuleEntry),
            ("src/main.js", PathClass::ModuleEntry),
            ("src/main.ts", PathClass::ModuleEntry),
            ("pkg/__main__.py", PathClass::ModuleEntry),
            // Generated: markers, protobuf conventions, codegen double
            // extensions. Pre-resolution variant; shadowing is resolved
            // against the index in prepare_context.
            ("src/foo.g.dart", PathClass::Generated),
            ("src/user.g.ts", PathClass::Generated),
            ("src/foo.pb.go", PathClass::Generated),
            ("src/api.generated.ts", PathClass::Generated),
            ("src/foo.gen.ts", PathClass::Generated),
            ("python/foo_pb2.py", PathClass::Generated),
            ("python/foo_pb2_grpc.py", PathClass::Generated),
            ("src/svc_generated.rs", PathClass::Generated),
            // Ordinary: everything else.
            ("src/wonk.rs", PathClass::Ordinary),
            ("src/parser.rs", PathClass::Ordinary),
            ("lib/core/service.py", PathClass::Ordinary),
        ];
        for (path, expected) in cases {
            assert_eq!(
                classify_path_character(std::path::Path::new(path)),
                *expected,
                "{path}"
            );
        }
    }

    #[test]
    fn classify_path_character_first_match_precedence() {
        // The ladder matches first-come in value order: a marker wins over
        // every other bucket (its demotion is only kept when a peer is
        // verified, else prepare_context restores Ordinary); the specific
        // path buckets win over the generic later ones.
        let cases: &[(&str, PathClass)] = &[
            ("tests/foo.d.ts", PathClass::Test),
            ("examples/index.ts", PathClass::Example),
            ("src/foo.g.dart", PathClass::Generated),
            ("tests/foo.pb.go", PathClass::Generated),
            ("src/index.g.ts", PathClass::Generated),
        ];
        for (path, expected) in cases {
            assert_eq!(
                classify_path_character(std::path::Path::new(path)),
                *expected,
                "{path}"
            );
        }
    }

    #[test]
    fn classify_path_character_dir_checks_match_directories_not_filenames() {
        // A directory named "testing" or a file named "compat" is not a
        // bucket: the dir checks read path components of the PARENT.
        assert_eq!(
            classify_path_character(std::path::Path::new("src/testing/foo.rs")),
            PathClass::Ordinary
        );
        assert_eq!(
            classify_path_character(std::path::Path::new("src/contest.rs")),
            PathClass::Ordinary
        );
        assert_eq!(
            classify_path_character(std::path::Path::new("src/spec.rs")),
            PathClass::Ordinary
        );
    }

    #[test]
    fn is_type_declaration_extension_set() {
        for path in ["src/foo.d.ts", "foo.d.mts", "foo.d.cts", "include/ffi.h"] {
            assert!(is_type_declaration(path), "{path}");
        }
        for path in [
            "src/foo.ts",
            "src/foo.hx",
            "src/dts.ts",
            "src/foo.htaccess",
            "src/foo",
        ] {
            assert!(!is_type_declaration(path), "{path}");
        }
    }

    #[test]
    fn is_generated_name_marker_set() {
        for path in [
            "src/api.generated.ts",
            "src/foo.gen.ts",
            "src/foo.pb.go",
            "src/user.g.dart",
            "src/user.g.ts",
            "python/foo_pb2.py",
            "python/foo_pb2_grpc.py",
            "src/svc_generated.rs",
        ] {
            assert!(is_generated_name(path), "{path}");
        }
        for path in [
            "src/foo.ts",
            "src/foo_pb.rs",
            "src/general.ts",
            "src/foo.go",
            "src/g.dart",
        ] {
            assert!(!is_generated_name(path), "{path}");
        }
    }

    #[test]
    fn strip_generated_marker_yields_the_peer_name() {
        assert_eq!(
            strip_generated_marker("user.g.dart").as_deref(),
            Some("user.dart")
        );
        assert_eq!(
            strip_generated_marker("api.generated.ts").as_deref(),
            Some("api.ts")
        );
        assert_eq!(
            strip_generated_marker("foo_pb2.py").as_deref(),
            Some("foo.py")
        );
        assert_eq!(
            strip_generated_marker("foo_pb2_grpc.py").as_deref(),
            Some("foo.py")
        );
        assert_eq!(
            strip_generated_marker("foo.pb.go").as_deref(),
            Some("foo.go")
        );
        assert_eq!(
            strip_generated_marker("foo.gen.ts").as_deref(),
            Some("foo.ts")
        );
        // No marker: no peer name.
        assert_eq!(strip_generated_marker("foo.ts"), None);
    }

    #[test]
    fn prepare_context_seeds_the_graded_classifier() {
        let results = vec![
            classified("src/a.rs", 1, "x", ResultCategory::Other),
            classified("tests/b.rs", 1, "x", ResultCategory::Other),
            classified("src/c.d.ts", 1, "x", ResultCategory::Other),
        ];
        let ctx = prepare_context(
            ContextReqs::none().with_path_class(),
            "x",
            &results,
            None,
            &ContextSources::default(),
        );
        assert_eq!(ctx.path_class("src/a.rs"), Some(PathClass::Ordinary));
        assert_eq!(ctx.path_class("tests/b.rs"), Some(PathClass::Test));
        assert_eq!(
            ctx.path_class("src/c.d.ts"),
            Some(PathClass::TypeDeclaration)
        );
    }

    // -------------------------------------------------------------------
    // Generated-peer shadowing (TASK-094, AC-2)
    // -------------------------------------------------------------------

    /// Seed the `files` table with the given paths (the index side of peer
    /// verification — candidates alone never verify a peer).
    fn seed_files(conn: &Connection, paths: &[&str]) {
        for path in paths {
            conn.execute(
                "INSERT INTO files (path, language, hash, last_indexed, line_count) \
                 VALUES (?1, 'rust', 'h', 0, 10)",
                rusqlite::params![path],
            )
            .unwrap();
        }
    }

    #[test]
    fn resolve_generated_shadowing_with_verified_peer() {
        let (dir, conn) = seeded_conn();
        seed_files(&conn, &["src/user.g.dart", "src/user.dart"]);
        let shadowed = resolve_generated_shadowing(Some(&conn), &["src/user.g.dart".to_string()]);
        assert_eq!(shadowed, HashSet::from(["src/user.g.dart".to_string()]));
        drop(dir);
    }

    #[test]
    fn resolve_generated_shadowing_without_peer_is_empty() {
        let (dir, conn) = seeded_conn();
        // No peer anywhere in the index.
        seed_files(&conn, &["src/user.g.dart"]);
        assert!(
            resolve_generated_shadowing(Some(&conn), &["src/user.g.dart".to_string()]).is_empty()
        );

        // A peer in a DIFFERENT directory is not a peer: same-named
        // hand-written files elsewhere say nothing about this one.
        seed_files(&conn, &["other/user.dart"]);
        assert!(
            resolve_generated_shadowing(Some(&conn), &["src/user.g.dart".to_string()]).is_empty()
        );

        // A peer that is itself generated is not hand-written.
        seed_files(&conn, &["src/a.gen.gen.ts", "src/a.gen.ts"]);
        assert!(
            resolve_generated_shadowing(Some(&conn), &["src/a.gen.gen.ts".to_string()]).is_empty()
        );
        drop(dir);
    }

    #[test]
    fn resolve_generated_shadowing_requires_the_index_not_the_candidate_set() {
        let (dir, conn) = seeded_conn();
        // The peer exists only among the CANDIDATES, not in the files
        // table: unverified — no demotion (AC-2 conservative read).
        seed_files(&conn, &["src/user.g.dart"]);
        let candidates = vec!["src/user.g.dart".to_string(), "src/user.dart".to_string()];
        assert!(resolve_generated_shadowing(Some(&conn), &candidates).is_empty());
        drop(dir);
    }

    #[test]
    fn resolve_generated_shadowing_without_conn_is_empty() {
        assert!(resolve_generated_shadowing(None, &["src/user.g.dart".to_string()]).is_empty());
    }

    #[test]
    fn prepare_context_rewrites_generated_by_verified_peer() {
        let (dir, conn) = seeded_conn();
        // Peer present: GeneratedShadowed (0.10).
        seed_files(&conn, &["src/user.g.dart", "src/user.dart"]);
        let results = vec![classified("src/user.g.dart", 1, "x", ResultCategory::Other)];
        let ctx = prepare_context(
            ContextReqs::none().with_path_class(),
            "x",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );
        assert_eq!(
            ctx.path_class("src/user.g.dart"),
            Some(PathClass::GeneratedShadowed)
        );

        // Peer absent from the index: back to Ordinary — never demoted
        // without a peer.
        let (dir2, conn2) = seeded_conn();
        seed_files(&conn2, &["src/user.g.dart"]);
        let ctx = prepare_context(
            ContextReqs::none().with_path_class(),
            "x",
            &results,
            Some(&conn2),
            &ContextSources::default(),
        );
        assert_eq!(ctx.path_class("src/user.g.dart"), Some(PathClass::Ordinary));
        drop(dir);
        drop(dir2);
    }

    #[test]
    fn prepare_context_generated_without_conn_stays_ordinary() {
        let results = vec![classified("src/user.g.dart", 1, "x", ResultCategory::Other)];
        let ctx = prepare_context(
            ContextReqs::none().with_path_class(),
            "x",
            &results,
            None,
            &ContextSources::default(),
        );
        assert_eq!(ctx.path_class("src/user.g.dart"), Some(PathClass::Ordinary));
    }

    // -------------------------------------------------------------------
    // PathCharacterSignal
    // -------------------------------------------------------------------

    #[test]
    fn path_character_signal_reads_the_graded_ladder_from_context() {
        // One peer-verified generated file plus one peerless one, so the
        // prepared context carries the full range of rewritten classes.
        let (dir, conn) = seeded_conn();
        seed_files(
            &conn,
            &["src/user.g.dart", "src/user.dart", "src/orphan.g.dart"],
        );
        let results = vec![
            classified("src/plain.rs", 1, "x", ResultCategory::Other),
            classified("tests/t.rs", 1, "x", ResultCategory::Other),
            classified("src/foo.d.ts", 1, "x", ResultCategory::Other),
            classified("compat/shim.ts", 1, "x", ResultCategory::Other),
            classified("examples/demo.ts", 1, "x", ResultCategory::Other),
            classified("src/index.ts", 1, "x", ResultCategory::Other),
            classified("src/main.rs", 1, "x", ResultCategory::Other),
            classified("src/user.g.dart", 1, "x", ResultCategory::Other),
            classified("src/orphan.g.dart", 1, "x", ResultCategory::Other),
        ];
        let ctx = prepare_context(
            ContextReqs::none().with_path_class(),
            "x",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );
        drop(dir);

        let signal = PathCharacterSignal;
        assert_eq!(signal.name(), "path_character");
        assert_eq!(signal.requires(), ContextReqs::none().with_path_class());
        let query = QueryInfo { pattern: "x" };
        let expected = [
            ("src/plain.rs", 1.00),
            ("tests/t.rs", 0.20),
            ("src/foo.d.ts", 0.30),
            ("compat/shim.ts", 0.45),
            ("examples/demo.ts", 0.60),
            ("src/index.ts", 0.70),
            ("src/main.rs", 0.80),
            ("src/user.g.dart", 0.10),
            ("src/orphan.g.dart", 1.00),
        ];
        for (file, value) in expected {
            let candidate = results
                .iter()
                .find(|r| r.result.file == Path::new(file))
                .unwrap();
            assert_eq!(
                signal.contribution(&query, candidate, &ctx),
                value,
                "{file}"
            );
        }
    }

    #[test]
    fn path_character_signal_unclassified_file_contributes_ordinary() {
        // Defensive branch: a candidate the context never classified is
        // never demoted (no evidence → the ordinary value).
        let signal = PathCharacterSignal;
        let candidate = classified("tests/t.rs", 1, "x", ResultCategory::Other);
        let query = QueryInfo { pattern: "x" };
        assert_eq!(
            signal.contribution(&query, &candidate, &SharedContext::default()),
            path_character_value(PathClass::Ordinary)
        );
    }

    // -------------------------------------------------------------------
    // Embedding context preparation (TASK-093)
    // -------------------------------------------------------------------

    /// A unit vector in the bundled provider's 256-dim space.
    fn bundled_unit_vector() -> Vec<f32> {
        let mut vector = vec![0.0f32; 256];
        vector[0] = 1.0;
        vector
    }

    /// `seeded_conn` plus a bundled-space embedding row for `my_func`
    /// (symbols.id 1 at src/main.rs:10).
    fn embedding_seeded_conn() -> (tempfile::TempDir, Connection) {
        let (dir, conn) = seeded_conn();
        let vector = bundled_unit_vector();
        conn.execute(
            "INSERT INTO embeddings \
             (symbol_id, file, chunk_text, vector, stale, created_at, provider, dim)
             VALUES (1, 'src/main.rs', 'my_func chunk', ?1, 0, 0, 'bundled', 256)",
            rusqlite::params![bytemuck::cast_slice(&vector)],
        )
        .unwrap();
        (dir, conn)
    }

    #[test]
    fn prepare_context_embeddings_populate_query_and_candidate_vectors() {
        let (_dir, conn) = embedding_seeded_conn();
        let results = vec![classified(
            "src/main.rs",
            10,
            "fn my_func() {}",
            ResultCategory::Definition,
        )];

        let ctx = prepare_context(
            ContextReqs::none().with_embeddings(),
            "my_func",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        let query = ctx.query_embedding().expect("query embedded once");
        assert_eq!(query.len(), 256, "bundled provider space");
        let candidate = ctx
            .embedding_at("src/main.rs", 10)
            .expect("candidate vector at the symbol position");
        assert_eq!(candidate.len(), 256);
        assert_eq!(candidate[0], 1.0);
        // The stored vector round-trips unit-normalized.
        let norm: f32 = candidate.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn prepare_context_embeddings_degrade_to_zero_on_foreign_space() {
        // Only ollama/768 rows exist while the configured provider is
        // bundled: the plan blocks, and the signal context must come back
        // empty (zero-path) instead of panicking or erroring.
        let (_dir, conn) = seeded_conn();
        let mut vector = vec![0.0f32; 768];
        vector[0] = 1.0;
        conn.execute(
            "INSERT INTO embeddings \
             (symbol_id, file, chunk_text, vector, stale, created_at, provider, dim)
             VALUES (1, 'src/main.rs', 'my_func chunk', ?1, 0, 0, 'ollama', 768)",
            rusqlite::params![bytemuck::cast_slice(&vector)],
        )
        .unwrap();
        let results = vec![classified(
            "src/main.rs",
            10,
            "fn my_func() {}",
            ResultCategory::Definition,
        )];

        let ctx = prepare_context(
            ContextReqs::none().with_embeddings(),
            "my_func",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        assert!(ctx.query_embedding().is_none());
        assert!(ctx.embedding_at("src/main.rs", 10).is_none());
    }

    #[test]
    fn prepare_context_embeddings_zero_when_no_rows() {
        // No embeddings anywhere: the loader returns nothing and the query
        // embed must never run (a successful bundled embed would otherwise
        // populate the query vector).
        let (_dir, conn) = seeded_conn();
        let results = vec![classified(
            "src/main.rs",
            10,
            "fn my_func() {}",
            ResultCategory::Definition,
        )];

        let ctx = prepare_context(
            ContextReqs::none().with_embeddings(),
            "my_func",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );

        assert!(ctx.query_embedding().is_none());
        assert!(ctx.embedding_at("src/main.rs", 10).is_none());
    }

    // -------------------------------------------------------------------
    // SemanticSignal
    // -------------------------------------------------------------------

    #[test]
    fn semantic_value_maps_cosine_range_absolutely() {
        assert_eq!(semantic_value(1.0), 1.0, "parallel");
        assert_eq!(semantic_value(0.0), 0.5, "orthogonal");
        assert_eq!(semantic_value(-1.0), 0.0, "anti-parallel");
        assert!((semantic_value(0.6) - 0.8).abs() < 1e-6);
        // Clamped, never extrapolated.
        assert_eq!(semantic_value(2.0), 1.0);
        assert_eq!(semantic_value(-2.0), 0.0);
        assert_eq!(semantic_value(f32::NAN), 0.0);
    }

    fn embed_ctx(query: Option<Vec<f32>>, vectors: Vec<(&str, u64, Vec<f32>)>) -> SharedContext {
        let mut ctx = SharedContext::default();
        ctx.embeddings.query = query;
        for (file, line, vector) in vectors {
            ctx.embeddings
                .vectors
                .insert((file.to_string(), line), vector);
        }
        ctx
    }

    #[test]
    fn semantic_signal_reads_direction_from_prepared_vectors() {
        let ctx = embed_ctx(
            Some(vec![1.0, 0.0]),
            vec![
                ("src/a.rs", 1, vec![1.0, 0.0]),
                ("src/b.rs", 1, vec![0.0, 1.0]),
                ("src/c.rs", 1, vec![-1.0, 0.0]),
            ],
        );

        let signal = SemanticSignal;
        assert_eq!(signal.name(), "semantic");
        assert_eq!(signal.requires(), ContextReqs::none().with_embeddings());
        let query = QueryInfo { pattern: "x" };
        for (file, expected) in [("src/a.rs", 1.0), ("src/b.rs", 0.5), ("src/c.rs", 0.0)] {
            let candidate = classified(file, 1, "x", ResultCategory::Definition);
            let value = signal.contribution(&query, &candidate, &ctx);
            assert!(
                (value - expected).abs() < 1e-6,
                "{file}: {value} vs {expected}"
            );
        }
    }

    #[test]
    fn semantic_signal_zero_when_query_missing_or_candidate_absent() {
        let signal = SemanticSignal;
        let query = QueryInfo { pattern: "x" };

        // Query present, candidate position carries no vector.
        let ctx = embed_ctx(Some(vec![1.0, 0.0]), vec![("src/a.rs", 1, vec![1.0, 0.0])]);
        let absent = classified("src/b.rs", 1, "x", ResultCategory::Definition);
        assert_eq!(signal.contribution(&query, &absent, &ctx), 0.0);

        // Query absent entirely (no embeddable source): still zero, never
        // an error.
        let no_query = embed_ctx(None, vec![("src/a.rs", 1, vec![1.0, 0.0])]);
        let present = classified("src/a.rs", 1, "x", ResultCategory::Definition);
        assert_eq!(signal.contribution(&query, &present, &no_query), 0.0);
    }

    #[test]
    fn semantic_signal_positive_for_embedded_definition() {
        // Full preparation path: the bundled provider embeds the query
        // in-process and the stored vector lives in the same space.
        let (_dir, conn) = embedding_seeded_conn();
        let results = vec![classified(
            "src/main.rs",
            10,
            "fn my_func() {}",
            ResultCategory::Definition,
        )];
        let ctx = prepare_context(
            ContextReqs::none().with_embeddings(),
            "my_func",
            &results,
            Some(&conn),
            &ContextSources::default(),
        );
        let signal = SemanticSignal;
        let query = QueryInfo { pattern: "my_func" };
        let value = signal.contribution(&query, &results[0], &ctx);
        assert!(
            value > 0.0,
            "same-space similarity must be positive: {value}"
        );
    }

    #[test]
    fn semantic_signal_absence_is_zero_not_penalty() {
        let (_dir, conn) = embedding_seeded_conn();
        let results = vec![
            classified(
                "src/main.rs",
                10,
                "fn my_func() {}",
                ResultCategory::Definition,
            ),
            classified("src/call.rs", 5, "my_func();", ResultCategory::CallSite),
            classified("src/note.rs", 2, "// my_func", ResultCategory::Comment),
        ];
        let query = QueryInfo { pattern: "my_func" };

        let kind_only = rerank_with_signals(
            vec![Box::new(KindSignal)],
            results.clone(),
            &query,
            Some(&conn),
            &table(&[("kind", 1.0)]),
            &ContextSources::default(),
        );
        let with_semantic = rerank_with_signals(
            vec![Box::new(KindSignal), Box::new(SemanticSignal)],
            results,
            &query,
            Some(&conn),
            &table(&[("kind", 1.0), ("semantic", 1.0)]),
            &ContextSources::default(),
        );

        let key = |s: &ScoredResult| {
            (
                s.classified.result.file.to_string_lossy().into_owned(),
                s.classified.result.line,
            )
        };
        let kind_keys: Vec<_> = kind_only.iter().map(key).collect();
        let semantic_keys: Vec<_> = with_semantic.iter().map(key).collect();
        // Absent candidates contribute exactly 0, so they cannot be
        // reordered past each other: the kind order survives intact.
        assert_eq!(semantic_keys, kind_keys);
        for scored in &with_semantic {
            if scored.classified.result.file != std::path::Path::new("src/main.rs") {
                assert_eq!(
                    scored.score,
                    kind_value(scored.classified.category),
                    "absent candidate must score its kind value exactly"
                );
            }
        }
        // The one embedded candidate only gained.
        assert!(with_semantic[0].score > kind_only[0].score);
    }

    // -------------------------------------------------------------------
    // Query-cost gate (PRD-RANK action: no additional queries per candidate)
    // -------------------------------------------------------------------

    static TRACE_STATEMENT_COUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn trace_stmt(event: rusqlite::trace::TraceEvent<'_>) {
        // trace_v2 takes a plain fn, so the counter lives in a static; only
        // the counting test installs a tracer, so there is no cross-talk.
        // The mask admits only SQLITE_TRACE_STMT, so every event counts.
        if matches!(event, rusqlite::trace::TraceEvent::Stmt(..)) {
            TRACE_STATEMENT_COUNT.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    fn count_context_statements(conn: &Connection, results: &[ClassifiedResult]) -> usize {
        TRACE_STATEMENT_COUNT.store(0, AtomicOrdering::SeqCst);
        conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            Some(trace_stmt),
        );
        let reqs = ContextReqs::none()
            .with_query_terms()
            .with_path_class()
            .with_symbol_hits()
            .with_lexical_scores()
            .with_embeddings();
        let _ = prepare_context(
            reqs,
            "alpha",
            results,
            Some(conn),
            &ContextSources::default(),
        );
        conn.trace_v2(rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT, None);
        TRACE_STATEMENT_COUNT.load(AtomicOrdering::SeqCst)
    }

    #[test]
    fn prepare_context_statement_count_independent_of_candidate_count() {
        // Signals take no Connection (structurally impossible to issue
        // per-candidate SQL); this gate proves it empirically. Accounting
        // for the one-term query "alpha" over lexical_seeded_conn (no
        // embedding rows): 8 statements total — 2 symbol-hit lookups
        // (symbols IN, references GROUP BY) + 4 lexical (presence probe,
        // corpus stats, 1 postings scan, document lengths) + 2 embedding
        // (stored vector spaces, position loader; the query embed itself
        // is in-process and SQL-free). query_terms and path_class touch
        // no SQL. The count must not move when the candidate set grows.
        let (_dir, conn) = lexical_seeded_conn();
        let make = |n: u64| -> Vec<ClassifiedResult> {
            (0..n)
                .map(|i| classified("a.rs", i + 1, "alpha", ResultCategory::Other))
                .collect()
        };

        let small = count_context_statements(&conn, &make(3));
        let large = count_context_statements(&conn, &make(30));

        assert_eq!(small, large, "statement count must be O(1) in candidates");
        assert!(large <= 10, "unexpected statements: {large}");
        assert_eq!(large, 8, "documented statement accounting (see comment)");
    }

    #[test]
    fn shared_dedup_and_group_work_over_scored_results() {
        // The ONE dedup/group implementation (ranker.rs generics) must
        // behave identically over pipeline output: imports collapse and
        // the definition gets the annotation.
        let results = vec![
            classified(
                "src/reexport1.rs",
                1,
                "pub use crate::foo;",
                ResultCategory::Import,
            ),
            classified(
                "src/lib.rs",
                10,
                "pub fn foo() {}",
                ResultCategory::Definition,
            ),
            classified(
                "src/reexport2.rs",
                1,
                "pub use crate::foo;",
                ResultCategory::Import,
            ),
            classified("src/main.rs", 5, "foo();", ResultCategory::CallSite),
        ];
        let scored = rerank(
            results,
            &QueryInfo { pattern: "foo" },
            None,
            &WeightTable::kind_dominant(),
            &ContextSources::default(),
        );

        let deduped = crate::ranker::dedup_reexports(scored, "foo");
        assert_eq!(deduped.len(), 2);
        assert_eq!(
            deduped[0].classified.annotation.as_deref(),
            Some("(+2 other locations)")
        );

        let groups = crate::ranker::group_by_category(deduped);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, ResultCategory::Definition);
        assert_eq!(groups[0].1.len(), 1);
        assert_eq!(groups[1].0, ResultCategory::CallSite);
        assert_eq!(groups[1].1.len(), 1);
    }

    // -------------------------------------------------------------------
    // TASK-095: query classification (PRD-RANK-REQ-007)
    // -------------------------------------------------------------------

    #[test]
    fn classify_query_worked_table() {
        let cases = [
            ("validateToken", QueryClass::Symbol),
            ("my_func", QueryClass::Symbol),
            ("cache", QueryClass::Symbol),
            ("internal/auth/token.go", QueryClass::Path),
            ("src\\lib.rs", QueryClass::Path),
            ("parse(input: &str)", QueryClass::Signature),
            ("Foo::bar", QueryClass::Signature),
            ("fn foo() -> u8", QueryClass::Signature),
            ("cache eviction", QueryClass::Conceptual),
            ("how does auth refresh", QueryClass::Conceptual),
            ("", QueryClass::Conceptual),
            ("query-class", QueryClass::Conceptual),
        ];
        for (query, expected) in cases {
            assert_eq!(
                classify_query(query),
                expected,
                "query {query:?} must classify as {expected:?}"
            );
        }
    }

    #[test]
    fn classify_query_trims_surrounding_whitespace() {
        assert_eq!(classify_query("  my_func  "), QueryClass::Symbol);
        assert_eq!(classify_query("\tcache eviction\n"), QueryClass::Conceptual);
        assert_eq!(classify_query("   "), QueryClass::Conceptual);
    }

    #[test]
    fn classify_query_first_match_precedence() {
        // Signature outranks path: a path-shaped fragment carrying a `::`
        // separator names a qualified symbol with its definition context.
        assert_eq!(classify_query("src/lib.rs::main"), QueryClass::Signature);
        // Path outranks symbol: `a/b` is not an identifier shape.
        assert_eq!(classify_query("a/b"), QueryClass::Path);
        // A bare lowercase word with no separators is a name lookup.
        assert_eq!(classify_query("refresh"), QueryClass::Symbol);
    }

    #[test]
    fn query_class_as_str_round_trips() {
        for class in [
            QueryClass::Symbol,
            QueryClass::Path,
            QueryClass::Signature,
            QueryClass::Conceptual,
        ] {
            let s = class.as_str();
            assert_eq!(s.parse::<QueryClass>().unwrap(), class, "{s} round-trips");
        }
    }

    #[test]
    fn query_class_from_str_rejects_unknown() {
        assert!("troll".parse::<QueryClass>().is_err());
        assert!("".parse::<QueryClass>().is_err());
        assert!("Symbol".parse::<QueryClass>().is_err(), "case-sensitive");
    }

    #[test]
    fn query_class_clap_value_enum_kebab_names() {
        for class in [
            QueryClass::Symbol,
            QueryClass::Path,
            QueryClass::Signature,
            QueryClass::Conceptual,
        ] {
            let value =
                clap::ValueEnum::to_possible_value(&class).expect("every variant has a clap value");
            assert_eq!(
                value.get_name(),
                class.as_str(),
                "clap name must equal as_str (kebab-case)"
            );
        }
    }

    // -------------------------------------------------------------------
    // TASK-095: per-class channel multipliers (PRD-RANK-REQ-008/009)
    // -------------------------------------------------------------------

    fn channel(lexical: f32, semantic: f32) -> ChannelMultipliers {
        ChannelMultipliers { lexical, semantic }
    }

    /// A raw search hit for the classed-entry tests (pre-classification).
    fn raw(file: &str, line: u64, content: &str) -> crate::search::SearchResult {
        crate::search::SearchResult {
            file: PathBuf::from(file),
            line,
            col: 1,
            content: content.to_string(),
        }
    }

    #[test]
    fn class_multipliers_neutral_is_all_ones() {
        let neutral = ClassMultipliers::neutral();
        assert_eq!(neutral.for_class(QueryClass::Symbol), channel(1.0, 1.0));
        assert_eq!(neutral.for_class(QueryClass::Path), channel(1.0, 1.0));
        assert_eq!(neutral.for_class(QueryClass::Signature), channel(1.0, 1.0));
        assert_eq!(ClassMultipliers::default(), neutral);
    }

    #[test]
    fn class_multipliers_conceptual_is_unconditionally_neutral() {
        // REQ-009: conceptual is the 1.0 baseline. There is no config entry
        // for it, so however the other classes are tuned, conceptual stays
        // pinned at 1.0/1.0.
        let skewed = ClassMultipliers {
            symbol: channel(3.0, 0.1),
            path: channel(0.0, 2.0),
            signature: channel(4.0, 0.5),
        };
        assert_eq!(skewed.for_class(QueryClass::Conceptual), channel(1.0, 1.0));
        assert_eq!(
            ClassMultipliers::neutral().for_class(QueryClass::Conceptual),
            channel(1.0, 1.0)
        );
    }

    #[test]
    fn class_multipliers_scale_only_lexical_and_semantic() {
        let weights = table(&[
            ("kind", 1.0),
            ("lexical", 0.8),
            ("semantic", 0.4),
            ("prominence", 0.6),
            ("centrality", 0.25),
            ("path_character", 0.3),
            ("proximity", 0.2),
            ("signature", 0.5),
        ]);
        let multipliers = ClassMultipliers {
            symbol: channel(1.5, 2.5),
            ..ClassMultipliers::neutral()
        };
        let effective = multipliers.apply(&weights, QueryClass::Symbol);
        assert_eq!(effective.weight("lexical"), 0.8 * 1.5);
        assert_eq!(effective.weight("semantic"), 0.4 * 2.5);
        // Structural signals are class-independent (REQ-008).
        assert_eq!(effective.weight("kind"), 1.0);
        assert_eq!(effective.weight("prominence"), 0.6);
        assert_eq!(effective.weight("centrality"), 0.25);
        assert_eq!(effective.weight("path_character"), 0.3);
        assert_eq!(effective.weight("proximity"), 0.2);
        assert_eq!(effective.weight("signature"), 0.5);
    }

    #[test]
    fn class_multipliers_conceptual_apply_is_bitwise_neutral() {
        let weights = table(&[("kind", 1.0), ("lexical", 0.8), ("semantic", 0.4)]);
        let skewed = ClassMultipliers {
            symbol: channel(9.0, 0.0),
            path: channel(0.0, 9.0),
            signature: channel(4.0, 4.0),
        };
        // Even a fully skewed table cannot move a conceptual query.
        assert_eq!(skewed.apply(&weights, QueryClass::Conceptual), weights);
    }

    #[test]
    fn rank_settings_from_config_maps_every_surface() {
        let mut rank = crate::config::RankConfig {
            enabled: true,
            weights: std::collections::HashMap::from([("kind".to_string(), 1.2)]),
            class_multipliers: ClassMultipliers {
                symbol: channel(1.5, 0.5),
                ..ClassMultipliers::neutral()
            },
        };
        let search = crate::config::SearchConfig {
            bm25_k1: 2.0,
            ..Default::default()
        };

        let settings = RankSettings::from_config(
            &rank,
            &search,
            crate::embedding::EmbeddingProviderKind::Bundled,
            None,
        )
        .unwrap();
        assert!(settings.use_pipeline);
        assert_eq!(settings.weights.weight("kind"), 1.2);
        assert_eq!(settings.sources.bm25.k1, 2.0);
        assert_eq!(settings.class_multipliers.symbol.lexical, 1.5);
        assert_eq!(settings.pinned_class, None);

        // The pin threads through unchanged.
        let pinned = RankSettings::from_config(
            &rank,
            &search,
            crate::embedding::EmbeddingProviderKind::Bundled,
            Some(QueryClass::Path),
        )
        .unwrap();
        assert_eq!(pinned.pinned_class, Some(QueryClass::Path));

        // A disabled config stays on the legacy path.
        rank.enabled = false;
        let legacy = RankSettings::from_config(
            &rank,
            &search,
            crate::embedding::EmbeddingProviderKind::Bundled,
            None,
        )
        .unwrap();
        assert!(!legacy.use_pipeline);

        // Invalid weights surface as errors from the same call.
        rank.weights.insert("nosuch_signal".to_string(), 1.0);
        assert!(
            RankSettings::from_config(
                &rank,
                &search,
                crate::embedding::EmbeddingProviderKind::Bundled,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn class_multipliers_absent_channels_stay_absent() {
        // A 0.0 multiplier on an absent channel inserts nothing: adjustment,
        // not creation (REQ-008's wording).
        let weights = table(&[("kind", 1.0)]);
        let zeroing = ClassMultipliers {
            symbol: channel(0.0, 0.0),
            ..ClassMultipliers::neutral()
        };
        let effective = zeroing.apply(&weights, QueryClass::Symbol);
        assert_eq!(effective, table(&[("kind", 1.0)]));
    }

    #[test]
    fn zero_multiplier_disables_the_channel_including_context_prep() {
        // lexical*0 must leave the lexical signal inactive: no contribution
        // row is produced (the pipeline filters on EFFECTIVE weights, so
        // zeroing the channel also skips its context preparation).
        let (_dir, conn) = lexical_seeded_conn();
        let results = vec![raw("a.rs", 1, "alpha"), raw("b.rs", 1, "alpha")];
        let settings = RankSettings {
            use_pipeline: true,
            weights: table(&[("kind", 1.0), ("lexical", 0.5), ("semantic", 0.5)]),
            class_multipliers: ClassMultipliers {
                symbol: channel(0.0, 1.0),
                ..ClassMultipliers::neutral()
            },
            ..Default::default()
        };
        let ranked = rank_and_explain_classed(&results, Some(&conn), "alpha", &settings);
        let scored = &ranked.groups[0].1;
        assert!(
            scored
                .iter()
                .all(|s| !s.contributions.iter().any(|c| c.signal == "lexical"))
        );
    }

    #[test]
    fn contribution_weight_records_the_effective_value() {
        // --why transparency: the weight shown is the applied (effective)
        // one — base weight scaled by the class multiplier.
        let (_dir, conn) = lexical_seeded_conn();
        let results = vec![raw("a.rs", 1, "alpha"), raw("b.rs", 1, "alpha")];
        let settings = RankSettings {
            use_pipeline: true,
            weights: table(&[("kind", 1.0), ("lexical", 0.8)]),
            class_multipliers: ClassMultipliers {
                symbol: channel(1.5, 1.0),
                ..ClassMultipliers::neutral()
            },
            ..Default::default()
        };
        let ranked = rank_and_explain_classed(&results, Some(&conn), "alpha", &settings);
        let lexical = ranked.groups[0].1[0]
            .contributions
            .iter()
            .find(|c| c.signal == "lexical")
            .expect("lexical active");
        assert!(
            (lexical.weight - 0.8f32 * 1.5f32).abs() < 1e-6,
            "effective weight recorded: {}",
            lexical.weight
        );
        assert!(
            (lexical.weighted - lexical.value * 0.8f32 * 1.5f32).abs() < 1e-6,
            "weighted uses the effective weight"
        );
    }

    #[test]
    fn rank_and_explain_classed_reports_detected_and_pinned_class() {
        let results = vec![raw("src/a.rs", 1, "alpha")];
        let settings = RankSettings {
            use_pipeline: true,
            weights: WeightTable::kind_dominant(),
            ..Default::default()
        };
        // Detected.
        let detected = rank_and_explain_classed(&results, None, "alpha", &settings);
        assert_eq!(detected.query_class, Some(QueryClass::Symbol));
        // Pinned — detection bypassed even for a conceptual-shaped query.
        let pinned_settings = RankSettings {
            pinned_class: Some(QueryClass::Symbol),
            ..settings.clone()
        };
        let pinned =
            rank_and_explain_classed(&results, None, "how does alpha work", &pinned_settings);
        assert_eq!(pinned.query_class, Some(QueryClass::Symbol));
        // Legacy path never classifies.
        let legacy_settings = RankSettings {
            use_pipeline: false,
            ..settings
        };
        let legacy = rank_and_explain_classed(&results, None, "alpha", &legacy_settings);
        assert_eq!(legacy.query_class, None);
    }

    #[test]
    fn rank_and_explain_delegates_to_the_classed_entry_groups() {
        let results = vec![raw("src/a.rs", 1, "alpha")];
        let settings = RankSettings {
            use_pipeline: true,
            weights: WeightTable::kind_dominant(),
            ..Default::default()
        };
        let classed = rank_and_explain_classed(&results, None, "alpha", &settings);
        let delegated = rank_and_explain(&results, None, "alpha", &settings);
        let key = |groups: &Vec<(ResultCategory, Vec<ScoredResult>)>| {
            groups
                .iter()
                .flat_map(|(_, items)| items.iter())
                .map(|s| {
                    (
                        s.classified.result.file.clone(),
                        s.classified.result.line,
                        s.score,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(key(&delegated), key(&classed.groups));
    }

    #[test]
    fn legacy_path_never_applies_multipliers() {
        // Byte-identical legacy ordering under any multiplier configuration:
        // the legacy branch must not even classify.
        let results = vec![
            raw("tests/t.rs", 9, "foo();"),
            raw("src/b.rs", 7, "use foo;"),
            raw("src/a.rs", 3, "fn foo() {}"),
            raw("src/a.rs", 12, "foo();"),
        ];
        let legacy = crate::ranker::rank_and_dedup(&results, None, "foo");
        let settings = RankSettings {
            use_pipeline: false,
            weights: WeightTable::kind_dominant(),
            class_multipliers: ClassMultipliers {
                symbol: channel(7.0, 0.0),
                path: channel(0.0, 7.0),
                signature: channel(5.0, 5.0),
            },
            ..Default::default()
        };
        let ranked = rank_and_explain_classed(&results, None, "foo", &settings);
        let flat: Vec<_> = ranked
            .groups
            .iter()
            .flat_map(|(_, items)| items.iter().map(|s| s.classified.clone()))
            .collect();
        let expected: Vec<_> = legacy.iter().flat_map(|(_, v)| v.clone()).collect();
        assert_eq!(flat, expected);
    }

    #[test]
    fn pinned_class_selects_the_multiplier_set() {
        // A pinned Symbol on an otherwise conceptual-shaped query must
        // behave exactly like a symbol-shaped query under the same
        // multipliers — the pin bypasses detection for scaling too.
        let (_dir, conn) = lexical_seeded_conn();
        let results = vec![raw("a.rs", 1, "alpha"), raw("b.rs", 1, "alpha")];
        let base = RankSettings {
            use_pipeline: true,
            weights: table(&[("kind", 1.0), ("lexical", 0.8)]),
            class_multipliers: ClassMultipliers {
                symbol: channel(1.5, 1.0),
                ..ClassMultipliers::neutral()
            },
            ..Default::default()
        };
        // Symbol-shaped query, no pin.
        let symbol = rank_and_explain_classed(&results, Some(&conn), "alpha", &base);
        // Conceptual-shaped query, pinned Symbol.
        let pinned = RankSettings {
            pinned_class: Some(QueryClass::Symbol),
            ..base.clone()
        };
        let as_pinned =
            rank_and_explain_classed(&results, Some(&conn), "how does alpha work", &pinned);
        let scores = |r: &RankedSearch| {
            r.groups
                .iter()
                .flat_map(|(_, items)| items.iter())
                .map(|s| s.score)
                .collect::<Vec<_>>()
        };
        assert_eq!(scores(&symbol), scores(&as_pinned));
    }

    // -------------------------------------------------------------------
    // TASK-095: headline AC pair + conceptual neutrality (unit level)
    // -------------------------------------------------------------------

    /// The conceptual query whose text b.rs's embedding carries.
    const AC_CONCEPTUAL_QUERY: &str = "how does auth refresh";

    /// The AC-pair fixture: `a.rs` is the exact-token match (dominant tf for
    /// every shared term; its stored embedding is the symbol text), `b.rs`
    /// is the semantically related doc note (set-minimum tf; its stored
    /// embedding IS the conceptual query — the file only a semantic channel
    /// can prefer).
    fn ac_pair_conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        for (path, lines) in [("a.rs", 40i64), ("b.rs", 60i64)] {
            conn.execute(
                "INSERT INTO files (path, language, hash, last_indexed, line_count) \
                 VALUES (?1, 'rust', 'h', 0, ?2)",
                rusqlite::params![path, lines],
            )
            .unwrap();
        }
        // Symbol rows at the candidate positions — the embedding loader
        // joins embeddings against them.
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) \
             VALUES ('validate_token', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) \
             VALUES ('refresh_note', 'comment', 'b.rs', 1, 0, 'rust')",
            [],
        )
        .unwrap();
        // Lexical: a.rs dominates every shared term, b.rs holds the set
        // minimum, so min-max normalization gives a.rs 1.0 and b.rs 0.0 for
        // BOTH queries (a degenerate one-file set would zero the signal).
        for (term, a_tf, b_tf) in [
            ("validate", 8i64, 1i64),
            ("token", 8, 1),
            ("auth", 6, 1),
            ("refresh", 6, 1),
            ("how", 2, 1),
            ("does", 2, 1),
        ] {
            conn.execute(
                "INSERT INTO term_stats (term, file, tf) VALUES (?1, 'a.rs', ?2)",
                rusqlite::params![term, a_tf],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO term_stats (term, file, tf) VALUES (?1, 'b.rs', ?2)",
                rusqlite::params![term, b_tf],
            )
            .unwrap();
        }
        // Embeddings in the bundled provider's own space.
        let provider = crate::bundled_embedding::BundledProvider;
        use crate::embedding::EmbeddingProvider as _;
        let mut a_vec = provider.embed_single("validate_token").unwrap();
        let mut b_vec = provider.embed_single(AC_CONCEPTUAL_QUERY).unwrap();
        crate::embedding::normalize(&mut a_vec);
        crate::embedding::normalize(&mut b_vec);
        conn.execute(
            "INSERT INTO embeddings \
             (symbol_id, file, chunk_text, vector, stale, created_at, provider, dim) \
             VALUES (1, 'a.rs', 'validate_token', ?1, 0, 0, 'bundled', 256)",
            rusqlite::params![bytemuck::cast_slice(&a_vec)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO embeddings \
             (symbol_id, file, chunk_text, vector, stale, created_at, provider, dim) \
             VALUES (2, 'b.rs', 'refresh note', ?1, 0, 0, 'bundled', 256)",
            rusqlite::params![bytemuck::cast_slice(&b_vec)],
        )
        .unwrap();
        (dir, conn)
    }

    fn ac_pair_candidates() -> Vec<crate::search::SearchResult> {
        vec![
            raw("a.rs", 1, "fn validate_token() {}"),
            raw("b.rs", 1, "/// refreshes access credentials periodically"),
        ]
    }

    /// The tuned-blend test table: semantic-dominant base (the conceptual
    /// intent), symbol multipliers flipping the blend lexical-dominant.
    fn ac_tuned_settings() -> RankSettings {
        RankSettings {
            use_pipeline: true,
            weights: table(&[("kind", 1.0), ("lexical", 0.3), ("semantic", 1.5)]),
            class_multipliers: ClassMultipliers {
                symbol: channel(2.0, 0.2),
                ..ClassMultipliers::neutral()
            },
            ..Default::default()
        }
    }

    #[test]
    fn symbol_query_ranks_exact_token_above_semantically_related() {
        let (_dir, conn) = ac_pair_conn();
        let results = ac_pair_candidates();
        let ranked = rank_and_explain_classed(
            &results,
            Some(&conn),
            "validate_token",
            &ac_tuned_settings(),
        );
        assert_eq!(ranked.query_class, Some(QueryClass::Symbol));
        let files: Vec<String> = ranked
            .groups
            .iter()
            .flat_map(|(_, items)| items.iter())
            .map(|s| s.classified.result.file.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            files,
            vec!["a.rs".to_string(), "b.rs".to_string()],
            "exact-token match must outrank the semantic note for a symbol query"
        );
    }

    #[test]
    fn conceptual_query_ranks_semantically_related_above_exact_token() {
        let (_dir, conn) = ac_pair_conn();
        let results = ac_pair_candidates();
        let ranked = rank_and_explain_classed(
            &results,
            Some(&conn),
            AC_CONCEPTUAL_QUERY,
            &ac_tuned_settings(),
        );
        assert_eq!(ranked.query_class, Some(QueryClass::Conceptual));
        let files: Vec<String> = ranked
            .groups
            .iter()
            .flat_map(|(_, items)| items.iter())
            .map(|s| s.classified.result.file.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            files,
            vec!["b.rs".to_string(), "a.rs".to_string()],
            "the semantic note must outrank the exact-token file for a conceptual query"
        );
    }

    #[test]
    fn conceptual_query_scores_identically_with_and_without_classification() {
        // REQ-009's AC: a conceptual query under the tuned (skewed) table
        // scores BIT-IDENTICALLY to the same query under neutral
        // multipliers — classification cannot move a conceptual query.
        let (_dir, conn) = ac_pair_conn();
        let results = ac_pair_candidates();
        let tuned = ac_tuned_settings();
        let neutral = RankSettings {
            class_multipliers: ClassMultipliers::neutral(),
            ..tuned.clone()
        };
        let with = rank_and_explain_classed(&results, Some(&conn), AC_CONCEPTUAL_QUERY, &tuned);
        let without =
            rank_and_explain_classed(&results, Some(&conn), AC_CONCEPTUAL_QUERY, &neutral);
        let scores = |r: &RankedSearch| {
            r.groups
                .iter()
                .flat_map(|(_, items)| items.iter())
                .map(|s| {
                    (
                        s.classified.result.file.clone(),
                        s.score,
                        s.contributions.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(scores(&with), scores(&without));
    }
}
