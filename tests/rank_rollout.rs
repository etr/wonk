//! Labeled ranking set and rollout gates (TASK-095, OQ-015/AR-034).
//!
//! The committed fixture at `tests/fixtures/labeled_queries` (a ~36-file
//! corpus with the M32 discriminating structures plus `labels.toml`) is
//! copied into a temp dir, indexed with the real pipeline, embedded with
//! the in-process bundled provider, and queried through the real
//! `text_search` → `rank_and_explain_classed` path. Precision@10 follows
//! the `ranking_regression` definition.
//!
//! The flip gates (REQ-017) live here too: the default flips only once
//! the tuned defaults beat the legacy ordering on this set, with no class
//! regressing, and the previous ordering stays reachable by config.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Deserialize;
use tempfile::TempDir;
use wonk::db;
use wonk::pipeline;
use wonk::rerank::{self, ClassMultipliers, QueryClass, RankSettings};
use wonk::search::{self, SearchResult};

const CORPUS: &str = "tests/fixtures/labeled_queries/corpus";
const LABELS: &str = "tests/fixtures/labeled_queries/labels.toml";

// ---------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Labels {
    query: Vec<LabeledQuery>,
}

#[derive(Debug, Deserialize)]
struct LabeledQuery {
    text: String,
    class: String,
    #[serde(default)]
    because: String,
    relevant: Vec<String>,
}

fn load_labels() -> Labels {
    let text = fs::read_to_string(LABELS).expect("labels.toml checked in");
    toml::from_str(&text).expect("labels.toml parses")
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Copy the labeled corpus into a temp dir, index it, embed it with the
/// bundled provider, and open the index.
fn setup_labeled_corpus() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    copy_dir(Path::new(CORPUS), root);
    fs::create_dir(root.join(".git")).unwrap();
    common::build_index(root, true).unwrap();
    let index_path = db::find_existing_index(root).expect("fixture index to exist");
    let conn = db::open(&index_path).unwrap();
    let provider = wonk::embedding::BundledProvider;
    pipeline::build_embeddings(&conn, root, &provider, wonk::progress::ProgressMode::Silent)
        .unwrap();
    (dir, conn)
}

/// Literal text search over the corpus, with root prefixes stripped so the
/// paths match walker-relative index keys.
fn candidates(root: &Path, query: &str) -> Vec<SearchResult> {
    let root_str = root.display().to_string();
    let mut results = search::text_search(query, false, false, &[root_str]).unwrap();
    for result in &mut results {
        if let Ok(rel) = result.file.strip_prefix(root) {
            result.file = rel.to_path_buf();
        }
    }
    results
}

/// The ranked top-10 files for one query under `settings` (groups
/// flattened in emission order).
fn ranked_files(
    root: &Path,
    conn: &Connection,
    query: &str,
    settings: &RankSettings,
) -> Vec<String> {
    let found = candidates(root, query);
    let ranked = rerank::rank_and_explain_classed(&found, Some(conn), query, settings);
    ranked
        .groups
        .iter()
        .flat_map(|(_, items)| items.iter())
        .map(|s| s.classified.result.file.to_string_lossy().into_owned())
        .take(10)
        .collect()
}

fn precision_at_10(ranked: &[String], relevant: &[String]) -> f32 {
    ranked
        .iter()
        .take(10)
        .filter(|f| relevant.contains(f))
        .count() as f32
        / 10.0
}

/// Mean precision@10 for `settings` over the whole labeled set: the
/// per-query mean (sum of p@10 over all 40 queries / 40) — the same
/// `ranking_regression` metric bench/rank_tune_bench.rs reports and
/// rank-tuning-results.md records — plus per-class means and the raw
/// per-query values.
fn measure(root: &Path, conn: &Connection, settings: &RankSettings) -> Summary {
    let labels = load_labels();
    let mut by_class: HashMap<String, (f32, usize)> = HashMap::new();
    let mut per_query = HashMap::new();
    let mut sum = 0.0f32;
    for query in &labels.query {
        let ranked = ranked_files(root, conn, &query.text, settings);
        let p = precision_at_10(&ranked, &query.relevant);
        sum += p;
        per_query.insert(query.text.clone(), p);
        let entry = by_class.entry(query.class.clone()).or_insert((0.0, 0));
        entry.0 += p;
        entry.1 += 1;
    }
    let mut per_class = HashMap::new();
    for (class, (class_sum, count)) in by_class {
        per_class.insert(class, class_sum / count as f32);
    }
    let overall = sum / labels.query.len() as f32;
    Summary {
        overall,
        per_class,
        per_query,
    }
}

#[derive(Debug, Clone)]
struct Summary {
    /// Mean precision@10 over all queries (the recorded bench metric).
    overall: f32,
    per_class: HashMap<String, f32>,
    per_query: HashMap<String, f32>,
}

/// The legacy ordering (REQ-017's baseline): the pre-pipeline sort.
fn legacy_settings() -> RankSettings {
    RankSettings::default()
}

/// The shipping [rank] defaults with the flip applied (enabled = true):
/// what every search runs after REQ-017's default flip.
fn tuned_defaults_as_shipped() -> RankSettings {
    let rank = wonk::config::RankConfig {
        enabled: true,
        ..Default::default()
    };
    RankSettings::from_config(
        &rank,
        &wonk::config::SearchConfig::default(),
        wonk::embedding::EmbeddingProviderKind::Bundled,
        None,
        true,
        0.85,
    )
    .unwrap()
}

/// A discrimination control for the flip gate: the shipped table with
/// every class-multiplier split inverted (lexical and semantic swapped
/// pairwise). Same weights, wrong direction — built from the shipped
/// settings so it always tracks future retunes.
fn inverted_split_settings() -> RankSettings {
    let shipped = tuned_defaults_as_shipped();
    let swap = |m: rerank::ChannelMultipliers| rerank::ChannelMultipliers {
        lexical: m.semantic,
        semantic: m.lexical,
    };
    RankSettings {
        class_multipliers: ClassMultipliers {
            symbol: swap(shipped.class_multipliers.symbol),
            path: swap(shipped.class_multipliers.path),
            signature: swap(shipped.class_multipliers.signature),
        },
        ..shipped
    }
}

// ---------------------------------------------------------------------------
// Fixture integrity (Phase 7)
// ---------------------------------------------------------------------------

const EXPECTED_QUOTAS: &[(&str, usize)] = &[
    ("symbol", 10),
    ("path", 8),
    ("signature", 8),
    ("conceptual", 14),
];

#[test]
fn labeled_set_is_checked_in_and_complete() {
    let labels = load_labels();
    assert_eq!(labels.query.len(), 40, "the labeled set holds 40 queries");

    let mut counts: HashMap<String, usize> = HashMap::new();
    for query in &labels.query {
        assert!(
            !query.because.is_empty(),
            "every label carries a justification (OQ-015): {}",
            query.text
        );
        assert!(
            !query.relevant.is_empty(),
            "every query labels at least one relevant file: {}",
            query.text
        );
        for file in &query.relevant {
            let path = PathBuf::from(CORPUS).join(file);
            assert!(
                path.exists(),
                "relevant file {file} for query {:?} must exist in the corpus",
                query.text
            );
        }
        *counts.entry(query.class.clone()).or_insert(0) += 1;
    }
    for (class, expected) in EXPECTED_QUOTAS {
        assert_eq!(
            counts.get(*class).copied().unwrap_or(0),
            *expected,
            "{class} quota"
        );
    }

    // Every query must produce at least one grep candidate over the corpus
    // (a query matching nothing measures nothing).
    let (dir, _conn) = setup_labeled_corpus();
    for query in &labels.query {
        let found = candidates(dir.path(), &query.text);
        assert!(
            !found.is_empty(),
            "query {:?} must match the corpus (labels are by construction)",
            query.text
        );
    }
}

#[test]
fn classifier_matches_labeled_classes() {
    // The classification regression suite: the class labels double as the
    // expected classify_query outputs.
    let labels = load_labels();
    for query in &labels.query {
        let expected: QueryClass = query
            .class
            .parse()
            .unwrap_or_else(|e| panic!("unknown class label {}: {e}", query.class));
        assert_eq!(
            rerank::classify_query(&query.text),
            expected,
            "query {:?} must classify as {expected:?}",
            query.text
        );
    }
}

// ---------------------------------------------------------------------------
// Rollout gates (Phase 8 adds the flip gate; the escape hatch lands early)
// ---------------------------------------------------------------------------

#[test]
fn previous_ordering_reachable_by_config() {
    // AR-033/REQ-017: an explicit [rank] enabled = false reproduces the
    // legacy ordering file-for-file, whatever the tuned defaults are.
    let (dir, conn) = setup_labeled_corpus();
    let labels = load_labels();
    let disabled = wonk::config::RankConfig {
        enabled: false,
        ..tuned_rank_config()
    };
    let escape = RankSettings::from_config(
        &disabled,
        &wonk::config::SearchConfig::default(),
        wonk::embedding::EmbeddingProviderKind::Bundled,
        None,
        true,
        0.85,
    )
    .unwrap();
    for query in &labels.query {
        let legacy = ranked_files(dir.path(), &conn, &query.text, &legacy_settings());
        let escaped = ranked_files(dir.path(), &conn, &query.text, &escape);
        assert_eq!(
            escaped, legacy,
            "query {:?}: enabled=false must reproduce the legacy ordering",
            query.text
        );
    }
}

/// The tuned [rank] defaults as they ship (transcribed verbatim from the
/// recorded tuning run, bench/rank-tuning-results.md candidate K).
fn tuned_rank_config() -> wonk::config::RankConfig {
    wonk::config::RankConfig::default()
}

#[test]
fn tuned_defaults_beat_legacy_on_labeled_set() {
    // REQ-017's standing flip gate: the shipping defaults (with the flip
    // applied) must beat the legacy ordering on the labeled set — mean
    // precision@10 over all 40 queries (the metric bench/rank_tune_bench
    // reports and rank-tuning-results.md records) strictly higher, no
    // query class regressing (legacy 0.5025 -> tuned 0.5175; symbol
    // +0.05, conceptual +0.0071, path and signature unchanged).
    //
    // Beating legacy alone cannot certify the tuning: at 40 queries a
    // p@10 step is 0.0025, so a wrong-but-harmless table also clears the
    // no-regression bar (an inverted symbol split measures 0.5125 and
    // used to pass). Three discriminators pin the gate to the recorded
    // tuning (bench/rank-tuning-results.md, candidate K):
    //   1. MARGIN PIN — the legacy and tuned means reproduce the recorded
    //      0.5025 / 0.5175 within three query-flip quanta (0.0075).
    //   2. INVERTED CONTROL — the same weights with every class
    //      multiplier split swapped (lexical <-> semantic) measures
    //      STRICTLY LOWER than the shipped tuning: the splits must point
    //      the right way, not merely avoid harm.
    //   3. The four recorded per-query improvements (retry_backoff,
    //      parse_header, TokenClaims, session storage) are present.
    let (dir, conn) = setup_labeled_corpus();
    let legacy = measure(dir.path(), &conn, &legacy_settings());
    let tuned = measure(dir.path(), &conn, &tuned_defaults_as_shipped());
    let inverted = measure(dir.path(), &conn, &inverted_split_settings());

    for (class, value) in &tuned.per_class {
        let baseline = legacy.per_class.get(class).copied().unwrap_or(0.0);
        assert!(
            *value >= baseline,
            "{class} regressed: tuned {value} vs legacy {baseline}"
        );
    }
    assert!(
        tuned.overall > legacy.overall,
        "mean precision@10 must strictly improve: tuned {} vs legacy {}",
        tuned.overall,
        legacy.overall
    );

    // 1. Margin pin: the measured means tie the gate to the recorded
    //    evidence (one p@10 flip at 40 queries moves the mean 0.0025;
    //    three quanta of tolerance absorb float summation only).
    const RECORDED_LEGACY_MEAN: f32 = 0.5025;
    const RECORDED_TUNED_MEAN: f32 = 0.5175;
    const QUERY_FLIP_QUANTUM: f32 = 0.0025;
    const TOLERANCE: f32 = 3.0 * QUERY_FLIP_QUANTUM;
    assert!(
        (tuned.overall - RECORDED_TUNED_MEAN).abs() <= TOLERANCE,
        "tuned mean {} must reproduce the recorded {} (bench/rank-tuning-results.md) \
         within {TOLERANCE}",
        tuned.overall,
        RECORDED_TUNED_MEAN
    );
    assert!(
        (legacy.overall - RECORDED_LEGACY_MEAN).abs() <= TOLERANCE,
        "legacy mean {} must reproduce the recorded {} (bench/rank-tuning-results.md) \
         within {TOLERANCE}",
        legacy.overall,
        RECORDED_LEGACY_MEAN
    );

    // 2. Inverted control: a table that anti-tunes the class splits must
    //    not measure as well as the shipped one.
    assert!(
        inverted.overall < tuned.overall,
        "the inverted-split control must measure strictly lower than the shipped \
         tuning: inverted {} vs tuned {}",
        inverted.overall,
        tuned.overall
    );

    // 3. The recorded per-query improvements must actually appear.
    for query in [
        "retry_backoff",
        "parse_header",
        "TokenClaims",
        "session storage",
    ] {
        let before = legacy.per_query[query];
        let after = tuned.per_query[query];
        assert!(
            after > before,
            "recorded improvement for {query:?} missing: legacy {before} -> tuned {after}"
        );
    }
}

#[test]
fn ac_pair_over_corpus_holds_under_shipped_defaults() {
    // The headline acceptance pair, pinned to the configuration users
    // actually receive (tuned_defaults_as_shipped: RankConfig::default()
    // with the flip applied) — not a bespoke table:
    //   - SYMBOL leg: for `validate_token`, the exact-token definition
    //     (src/auth/token.rs) ranks strictly above the same-named compat
    //     stub (src/compat/legacy_auth.rs), and the relevant files supply
    //     the top-10: only the tolerated compat mention may intrude
    //     (p@10 >= 0.9). The strength clause is the mutation check —
    //     under an inverted symbol split the mention still trails the
    //     definition, but a passer-by file enters the top-10 and drops
    //     p@10 to 0.8.
    //   - CONCEPTUAL leg: for `session storage`, the labeled topical
    //     files (src/auth/session.rs, src/notes/storage_notes.rs) outrank
    //     the incidental exact-token mention (src/misc/a_auth_notes.rs) —
    //     a file the query text literally matches (case-sensitively, as
    //     the candidate grep runs) but that is not an answer.
    // Neither leg may pass vacuously: a missing fixture file panics
    // instead of skipping its assertion.
    let (dir, conn) = setup_labeled_corpus();
    let settings = tuned_defaults_as_shipped();

    // --- symbol leg -------------------------------------------------------
    let ranked = ranked_files(dir.path(), &conn, "validate_token", &settings);
    let position = |file: &str| ranked.iter().position(|f| f == file);
    let definition = position("src/auth/token.rs")
        .unwrap_or_else(|| panic!("definition file missing from results: {ranked:?}"));
    let mention = position("src/compat/legacy_auth.rs")
        .unwrap_or_else(|| panic!("compat mention file missing from results: {ranked:?}"));
    assert!(
        definition < mention,
        "the exact-token definition must outrank the compat mention: {ranked:?}"
    );
    let symbol_relevant: Vec<String> = [
        "src/auth/token.rs",
        "tests/token_test.rs",
        "src/auth/session.rs",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let symbol_p10 = precision_at_10(&ranked, &symbol_relevant);
    assert!(
        symbol_p10 >= 0.9,
        "the definition must supply the top-10 (only the compat mention may \
         intrude): p@10 {symbol_p10}, ranked {ranked:?}"
    );

    // --- conceptual leg ---------------------------------------------------
    let ranked = ranked_files(dir.path(), &conn, "session storage", &settings);
    let position = |file: &str| ranked.iter().position(|f| f == file);
    let conceptual_relevant = ["src/auth/session.rs", "src/notes/storage_notes.rs"];
    for file in conceptual_relevant {
        position(file).unwrap_or_else(|| {
            panic!("relevant topical file {file} missing from top-10: {ranked:?}")
        });
    }
    let last_relevant_row = ranked
        .iter()
        .rposition(|f| conceptual_relevant.contains(&f.as_str()))
        .expect("a relevant row exists (presence asserted above)");
    let mentions = ["src/misc/a_auth_notes.rs"];
    // The mentions must be real candidates: the ranking demoted them, the
    // grep did not skip them.
    let found = candidates(dir.path(), "session storage");
    for file in mentions {
        assert!(
            found.iter().any(|r| r.file == Path::new(file)),
            "incidental mention {file} must be a grep candidate for the leg to mean anything"
        );
        if let Some(m) = position(file) {
            assert!(
                m > last_relevant_row,
                "incidental mention {file} at row {m} outranks the topical files \
                 (last relevant row {last_relevant_row}): {ranked:?}"
            );
        }
    }
}
