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

    fn union(self, other: Self) -> Self {
        Self {
            query_terms: self.query_terms || other.query_terms,
            path_class: self.path_class || other.path_class,
            symbol_hits: self.symbol_hits || other.symbol_hits,
            lexical_scores: self.lexical_scores || other.lexical_scores,
            embeddings: self.embeddings || other.embeddings,
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

/// Escape a literal for a SQLite `LIKE ... ESCAPE '\'` pattern.
fn escape_like_pattern(literal: &str) -> String {
    let mut escaped = String::with_capacity(literal.len());
    for c in literal.chars() {
        if matches!(c, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Which of `files` are generated names shadowing a hand-written peer
/// VERIFIED IN THE INDEX (the `files` table) — never the candidate set:
/// the peer may simply not match the query.
///
/// A peer is a file with the marker-stripped name in the same directory
/// (`src/user.g.dart` ← `src/user.dart`) that is not itself generated.
/// Resolution costs ONE prepared query per unique parent directory
/// (LIKE-escaped directory prefix); a failing prepare degrades to ONE full
/// `files` scan. No connection or no generated candidates → no demotion
/// (the conservative branch of AC-2).
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

    // One statement per unique parent directory; on any failure, one full
    // scan instead — either way the resolution is O(1) queries.
    let mut found: HashSet<String> = HashSet::new();
    let mut dirs: Vec<String> = Vec::new();
    for (_, peer) in &wanted {
        let dir = match peer.rfind('/') {
            Some(idx) => peer[..idx].to_string(),
            None => String::new(),
        };
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    let mut resolved = false;
    for dir in &dirs {
        let pattern = format!("{}%", escape_like_pattern(dir));
        if let Ok(mut stmt) = conn.prepare("SELECT path FROM files WHERE path LIKE ?1 ESCAPE '\\'")
            && let Ok(rows) =
                stmt.query_map(rusqlite::params![pattern], |row| row.get::<_, String>(0))
        {
            found.extend(rows.flatten());
            resolved = true;
        }
    }
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

/// Whether `term` occurs in `line` as a MAXIMAL identifier run
/// (`[A-Za-z0-9_]+` bounded by non-identifier characters),
/// case-insensitively.
///
/// A dedicated scanner rather than `tokenizer::tokenize`: the tokenizer
/// splits on `_`, so it would token-match `foo` inside `foo_bar` — here the
/// boundary is the point, because a query term appearing inside a longer
/// identifier names a different symbol. Terms containing non-ASCII
/// characters can never match (code identifiers are ASCII runs).
pub fn contains_identifier_token(line: &str, term: &str) -> bool {
    if term.is_empty() {
        return false;
    }
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|run| run.eq_ignore_ascii_case(term))
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
        // TASK-094: the seeding IS the graded classifier (the one path
        // signal); generated files are resolved against the index —
        // GeneratedShadowed with a verified peer, Ordinary without one
        // (never demoted without a peer).
        let files: Vec<String> = results
            .iter()
            .map(|r| r.result.file.to_string_lossy().into_owned())
            .collect();
        let shadowed = resolve_generated_shadowing(conn, &files);
        for file in files {
            let class = match classify_path_character(Path::new(&file)) {
                PathClass::Generated if shadowed.contains(&file) => PathClass::GeneratedShadowed,
                PathClass::Generated => PathClass::Ordinary,
                class => class,
            };
            ctx.path_class.entry(file).or_insert(class);
        }
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
}

impl Default for RankSettings {
    fn default() -> Self {
        Self {
            use_pipeline: false,
            weights: WeightTable::kind_dominant(),
            sources: ContextSources::default(),
        }
    }
}

/// Unified ranking entry point for search results.
///
/// Classification always happens in `ranker.rs`; then either the legacy
/// lexicographic sort (wrapped as unscored `ScoredResult`s — rendering
/// reads `.classified`, so output is byte-identical to today) or the
/// signal pipeline, followed by the ONE shared dedup/group implementation.
pub fn rank_and_explain(
    results: &[crate::search::SearchResult],
    conn: Option<&Connection>,
    pattern: &str,
    settings: &RankSettings,
) -> Vec<(ResultCategory, Vec<ScoredResult>)> {
    let classified = crate::ranker::classify_results(results, conn);
    let ranked = if settings.use_pipeline {
        let scored = rerank(
            classified,
            &QueryInfo { pattern },
            conn,
            &settings.weights,
            &settings.sources,
        );
        // Score order interleaves categories under any non-kind-only
        // weight table (group_by_category groups by adjacency); bucket
        // into tier order first so every category is emitted exactly
        // once. For kind-only positive weights this is the identity
        // permutation, so equivalence with the legacy output is exact.
        crate::ranker::bucket_by_category(scored)
    } else {
        crate::ranker::rank_results(classified)
            .into_iter()
            .map(|classified| ScoredResult {
                classified,
                score: 0.0,
                contributions: Vec::new(),
            })
            .collect()
    };
    let deduped = crate::ranker::dedup_reexports(ranked, pattern);
    crate::ranker::group_by_category(deduped)
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
    fn registry_contains_six_signals_in_order() {
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
                "path_character"
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
        let weights = table(&[("kind", 1.0)]);

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
            err.contains("known: kind, lexical, semantic, centrality, prominence, path_character"),
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
}
