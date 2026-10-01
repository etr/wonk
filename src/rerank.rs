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

/// Path classification bucket. Graded buckets arrive with TASK-094; today
/// the choice is binary, seeded from `ranker::is_test_file`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    /// A regular source file.
    Ordinary,
    /// A file matching the test-path heuristics.
    Test,
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
// Wired into builtin_signals() with the TASK-093 registry; until then only
// the unit tests construct it.
#[allow(dead_code)]
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
// Wired into builtin_signals() with the TASK-093 registry; until then only
// the unit tests construct it.
#[allow(dead_code)]
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

/// Registry of built-in signals. TASK-093/094 append entries here; config
/// name validation derives from this list, so new signals are accepted by
/// `[rank.weights]` automatically.
pub fn builtin_signals() -> Vec<Box<dyn Signal>> {
    vec![Box::new(KindSignal)]
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
        for r in results {
            let file = r.result.file.to_string_lossy().into_owned();
            let class = if crate::ranker::is_test_file(&r.result.file) {
                PathClass::Test
            } else {
                PathClass::Ordinary
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
    fn registry_contains_kind_and_names_derive_from_it() {
        let registry = builtin_signals();
        assert_eq!(registry.len(), 1);
        assert_eq!(registry[0].name(), "kind");
        assert_eq!(known_signal_names(), vec!["kind"]);
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
        weights.insert("lexical".to_string(), 1.0);
        let err = WeightTable::from_config(&weights).unwrap_err().to_string();
        assert!(
            err.contains("unknown signal name 'lexical'"),
            "error names the offender: {err}"
        );
        assert!(
            err.contains("known: kind"),
            "error lists the valid names: {err}"
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
