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

    fn union(self, other: Self) -> Self {
        Self {
            query_terms: self.query_terms || other.query_terms,
            path_class: self.path_class || other.path_class,
            symbol_hits: self.symbol_hits || other.symbol_hits,
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

/// Context prepared once per candidate set and shared by all signals.
#[derive(Debug, Default, Clone)]
pub struct SharedContext {
    pub(crate) terms: Vec<String>,
    pub(crate) path_class: HashMap<String, PathClass>,
    pub(crate) symbol_hits: HashMap<(String, u64), SymbolHit>,
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
) -> Vec<ScoredResult> {
    rerank_with_signals(builtin_signals(), results, query, conn, weights)
}

/// `rerank` over an explicit signal list (the test seam for spy signals).
pub(crate) fn rerank_with_signals(
    signals: Vec<Box<dyn Signal>>,
    results: Vec<ClassifiedResult>,
    query: &QueryInfo<'_>,
    conn: Option<&Connection>,
    weights: &WeightTable,
) -> Vec<ScoredResult> {
    let active: Vec<&Box<dyn Signal>> = signals
        .iter()
        .filter(|s| weights.weight(s.name()) != 0.0)
        .collect();
    let reqs = union_reqs(&signals, weights);
    let ctx = prepare_context(reqs, query.pattern, &results, conn);

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
}

impl Default for RankSettings {
    fn default() -> Self {
        Self {
            use_pipeline: false,
            weights: WeightTable::kind_dominant(),
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
        rerank(classified, &QueryInfo { pattern }, conn, &settings.weights)
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

        let scored = rerank_with_signals(signals, results, &query, None, &weights);
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

        let scored = rerank_with_signals(signals, results, &query, None, &weights);
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
        let a = rerank(make(), &query, None, &WeightTable::kind_dominant());
        let b = rerank(swapped, &query, None, &WeightTable::kind_dominant());
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

        let ctx = prepare_context(ContextReqs::none(), "my_func", &results, Some(&conn));

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
        let ctx = prepare_context(ContextReqs::none().with_path_class(), "x", &results, None);
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
        );
        assert_eq!(ctx.terms(), &["cache".to_string(), "eviction".to_string()]);
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
