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

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Deserialize;
use tempfile::TempDir;
use wonk::db;
use wonk::pipeline;
use wonk::rerank::{self, ClassMultipliers, QueryClass, RankSettings, WeightTable};
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
    pipeline::build_index(root, true).unwrap();
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

/// Mean precision@10 per query class and overall for `settings` over the
/// whole labeled set.
fn measure(root: &Path, conn: &Connection, settings: &RankSettings) -> Summary {
    let labels = load_labels();
    let mut by_class: HashMap<String, (f32, usize)> = HashMap::new();
    for query in &labels.query {
        let ranked = ranked_files(root, conn, &query.text, settings);
        let p = precision_at_10(&ranked, &query.relevant);
        let entry = by_class.entry(query.class.clone()).or_insert((0.0, 0));
        entry.0 += p;
        entry.1 += 1;
    }
    let mut per_class = HashMap::new();
    for (class, (sum, count)) in by_class {
        per_class.insert(class, sum / count as f32);
    }
    let overall = per_class.values().sum::<f32>() / per_class.len() as f32;
    Summary { overall, per_class }
}

#[derive(Debug, Clone)]
struct Summary {
    overall: f32,
    per_class: HashMap<String, f32>,
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
    )
    .unwrap()
}

fn weights_of(entries: &[(&str, f32)]) -> WeightTable {
    WeightTable::from_config(&entries.iter().map(|(n, w)| (n.to_string(), *w)).collect()).unwrap()
}

fn multipliers_of(symbol: (f32, f32), path: (f32, f32), signature: (f32, f32)) -> ClassMultipliers {
    ClassMultipliers {
        symbol: rerank::ChannelMultipliers {
            lexical: symbol.0,
            semantic: symbol.1,
        },
        path: rerank::ChannelMultipliers {
            lexical: path.0,
            semantic: path.1,
        },
        signature: rerank::ChannelMultipliers {
            lexical: signature.0,
            semantic: signature.1,
        },
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
    let mut disabled = wonk::config::RankConfig {
        enabled: false,
        ..tuned_rank_config()
    };
    disabled.enabled = false;
    let escape = RankSettings::from_config(
        &disabled,
        &wonk::config::SearchConfig::default(),
        wonk::embedding::EmbeddingProviderKind::Bundled,
        None,
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
    // precision@10 strictly higher, no query class regressing. The
    // measured numbers are recorded in bench/rank-tuning-results.md
    // (legacy 0.5025 vs tuned 0.5175; symbol +0.05, conceptual +0.0071,
    // path and signature unchanged).
    let (dir, conn) = setup_labeled_corpus();
    let legacy = measure(dir.path(), &conn, &legacy_settings());
    let tuned = measure(dir.path(), &conn, &tuned_defaults_as_shipped());

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
}

#[test]
fn symbol_query_over_corpus_prefers_the_definition_above_doc_mentions() {
    // The headline AC over the labeled corpus: a symbol query's exact-token
    // definition outranks prose mentions; a conceptual query's topical
    // files outrank incidental mentions.
    let (dir, conn) = setup_labeled_corpus();
    let settings = RankSettings {
        use_pipeline: true,
        weights: weights_of(&[
            ("kind", 1.0),
            ("lexical", 0.3),
            ("semantic", 1.2),
            ("prominence", 1.0),
            ("centrality", 0.3),
        ]),
        class_multipliers: multipliers_of((2.0, 0.2), (1.0, 1.0), (1.0, 1.0)),
        ..Default::default()
    };

    let ranked = ranked_files(dir.path(), &conn, "validate_token", &settings);
    let position = |file: &str| ranked.iter().position(|f| f == file);
    let definition = position("src/auth/token.rs");
    let mention = position("src/compat/legacy_auth.rs");
    match (definition, mention) {
        (Some(d), Some(m)) => assert!(
            d < m,
            "the exact-token definition must outrank the compat doc mention: {ranked:?}"
        ),
        (None, _) => panic!("definition file missing from results: {ranked:?}"),
        _ => {}
    }
}
