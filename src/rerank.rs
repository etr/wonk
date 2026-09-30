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
}
