//! Equivalence suite for the kind-signal pipeline (TASK-092, AR-033).
//!
//! The foundation contract for M32-M36: a kind-only configuration
//! reproduces the legacy ranker byte-for-byte. The harness mirrors
//! `ranking_regression.rs` — the committed fixture corpus is copied into a
//! temp dir, indexed with the real pipeline, and queried through the real
//! `text_search` path — so equivalence is asserted over the same data the
//! BM25 suite freezes.

use std::fs;
use std::path::Path;

use rusqlite::Connection;
use tempfile::TempDir;
use wonk::config::Config;
use wonk::db;
use wonk::pipeline;
use wonk::ranker::{self, ClassifiedResult, ResultCategory};
use wonk::rerank::{self, RankSettings, WeightTable};
use wonk::search::{self, SearchResult};

const FIXTURE: &str = "tests/fixtures/bm25_corpus";

/// The frozen query set from the BM25 regression suite.
const QUERIES: &[&str] = &[
    "cache",
    "retry",
    "parser",
    "token",
    "queue",
    "cache eviction",
    "retry backoff",
    "parser token",
];

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

/// Copy the fixture corpus into a temp dir, index it, and open the index.
fn setup_indexed_corpus() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    copy_dir(Path::new(FIXTURE), root);
    fs::create_dir(root.join(".git")).unwrap();
    pipeline::build_index(root, true).unwrap();
    let index_path = db::find_existing_index(root).expect("fixture index to exist");
    let conn = db::open(&index_path).unwrap();
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

fn pipeline_groups(
    results: &[SearchResult],
    conn: Option<&Connection>,
    query: &str,
) -> Vec<(ResultCategory, Vec<rerank::ScoredResult>)> {
    rerank::rank_and_explain(
        results,
        conn,
        query,
        &RankSettings {
            use_pipeline: true,
            weights: WeightTable::kind_dominant(),
        },
    )
}

fn flattened_pipeline(
    groups: &[(ResultCategory, Vec<rerank::ScoredResult>)],
) -> Vec<ClassifiedResult> {
    groups
        .iter()
        .flat_map(|(_, items)| items.iter().map(|s| s.classified.clone()))
        .collect()
}

fn flattened_legacy(groups: &[(ResultCategory, Vec<ClassifiedResult>)]) -> Vec<ClassifiedResult> {
    groups.iter().flat_map(|(_, items)| items.clone()).collect()
}

/// Assert the pipeline's grouped output structurally equals the legacy
/// ranker's: same group count, same category per group, and per-item
/// `ClassifiedResult` equality (which makes rendering byte-identical).
fn assert_equivalent(
    found: &[SearchResult],
    conn: Option<&Connection>,
    query: &str,
    settings: &RankSettings,
) {
    let legacy = ranker::rank_and_dedup(found, conn, query);
    let piped = rerank::rank_and_explain(found, conn, query, settings);

    assert_eq!(
        piped.len(),
        legacy.len(),
        "{query}: group count differs (conn={})",
        conn.is_some()
    );
    for (i, (p, l)) in piped.iter().zip(&legacy).enumerate() {
        assert_eq!(p.0, l.0, "{query}: group {i} category differs");
    }
    let fp = flattened_pipeline(&piped);
    let fl = flattened_legacy(&legacy);
    assert_eq!(
        fp,
        fl,
        "{query}: per-item results differ (conn={})",
        conn.is_some()
    );
}

// ---------------------------------------------------------------------------
// 1. Corpus equivalence, with and without a DB connection
// ---------------------------------------------------------------------------

#[test]
fn kind_only_pipeline_matches_legacy_over_corpus() {
    let (dir, conn) = setup_indexed_corpus();
    let root = dir.path();
    let settings = RankSettings {
        use_pipeline: true,
        weights: WeightTable::kind_dominant(),
    };
    for &query in QUERIES {
        let found = candidates(root, query);
        assert!(!found.is_empty(), "query {query} must have candidates");
        assert_equivalent(&found, Some(&conn), query, &settings);
        assert_equivalent(&found, None, query, &settings);
    }
}

// ---------------------------------------------------------------------------
// 2. Kind-weight invariance
// ---------------------------------------------------------------------------

#[test]
fn kind_weight_scaling_does_not_change_ordering() {
    let (dir, conn) = setup_indexed_corpus();
    let root = dir.path();
    for &query in QUERIES {
        let found = candidates(root, query);
        for weight in [1.0f32, 2.0, 0.5, 7.0] {
            let settings = RankSettings {
                use_pipeline: true,
                weights: WeightTable::from_config(&std::collections::HashMap::from([(
                    "kind".to_string(),
                    weight,
                )]))
                .unwrap(),
            };
            assert_equivalent(&found, Some(&conn), query, &settings);
            assert_equivalent(&found, None, query, &settings);
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Hand-built all-category matrix
// ---------------------------------------------------------------------------

/// All six categories, same-file multi-line ordering, duplicate (file,line)
/// stability, and the definition+2-imports dedup annotation — the cases the
/// corpus lacks (no use lines, no tests/ dirs).
fn matrix_results() -> Vec<SearchResult> {
    let mk = |file: &str, line: u64, content: &str| SearchResult {
        file: std::path::PathBuf::from(file),
        line,
        col: 1,
        content: content.to_string(),
    };
    vec![
        mk("tests/t.rs", 5, "my_func();"),
        mk("src/cmt.rs", 1, "// my_func note"),
        mk("src/misc.rs", 1, "let s = \"my_func\";"),
        mk("src/import2.rs", 1, "import { my_func } from './lib';"),
        mk("src/main.rs", 20, "let x = my_func();"),
        mk("src/main.rs", 20, "let x = my_func();"),
        mk("src/main.rs", 25, "let y = my_func() + 1;"),
        mk("src/main.rs", 30, "let z = my_func() + 2;"),
        mk("src/lib.rs", 10, "pub fn my_func() {}"),
        mk("src/import.rs", 1, "use crate::my_func;"),
    ]
}

fn seeded_matrix_conn() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let conn = db::open(&dir.path().join("index.db")).unwrap();
    conn.execute(
        "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["my_func", "function", "src/lib.rs", 10, 0, "rust"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO \"references\" (name, file, line, col, context) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params!["my_func", "src/main.rs", 20, 4, "let x = my_func();"],
    )
    .unwrap();
    (dir, conn)
}

#[test]
fn hand_built_matrix_matches_legacy_including_dedup() {
    let (_dir, conn) = seeded_matrix_conn();
    let found = matrix_results();
    let settings = RankSettings {
        use_pipeline: true,
        weights: WeightTable::kind_dominant(),
    };
    assert_equivalent(&found, Some(&conn), "my_func", &settings);
    assert_equivalent(&found, None, "my_func", &settings);

    // Structure spot-checks so the matrix is proven to exercise the
    // interesting paths (not vacuously equal on a degenerate set).
    let groups = pipeline_groups(&found, Some(&conn), "my_func");
    let flat = flattened_pipeline(&groups);
    // Both imports collapsed; definition annotated.
    assert_eq!(
        flat.iter()
            .find(|c| c.category == ResultCategory::Definition)
            .and_then(|c| c.annotation.as_deref()),
        Some("(+2 other locations)")
    );
    assert!(flat.iter().all(|c| c.category != ResultCategory::Import));
    // Duplicate (file,line) rows both survive and stay adjacent (stable
    // ordering inside the call-site group).
    let dup_positions: Vec<usize> = flat
        .iter()
        .enumerate()
        .filter(|(_, c)| c.result.file.to_string_lossy() == "src/main.rs" && c.result.line == 20)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(dup_positions.len(), 2, "both duplicate rows survive");
    assert_eq!(
        dup_positions[1] - dup_positions[0],
        1,
        "duplicates must stay adjacent: {dup_positions:?}"
    );
    // All six categories present in the input classification.
    let cats: Vec<ResultCategory> = groups.iter().map(|(c, _)| *c).collect();
    assert!(cats.contains(&ResultCategory::Definition));
    assert!(cats.contains(&ResultCategory::CallSite));
    assert!(cats.contains(&ResultCategory::Comment));
    assert!(cats.contains(&ResultCategory::Other));
    assert!(cats.contains(&ResultCategory::Test));
}

// ---------------------------------------------------------------------------
// 4. Legacy wrapper guard
// ---------------------------------------------------------------------------

#[test]
fn legacy_settings_reproduce_rank_and_dedup() {
    let (dir, conn) = setup_indexed_corpus();
    let root = dir.path();
    let settings = RankSettings::default();
    assert!(
        !settings.use_pipeline,
        "default settings stay on the legacy path"
    );
    for &query in QUERIES {
        let found = candidates(root, query);
        assert_equivalent(&found, Some(&conn), query, &settings);
        assert_equivalent(&found, None, query, &settings);
    }
}

// ---------------------------------------------------------------------------
// 5. Default-config gate (REQ-017)
// ---------------------------------------------------------------------------

#[test]
fn default_config_gates_to_legacy_ordering() {
    let config = Config::default();
    assert!(!config.rank.enabled, "pipeline must be disabled by default");
    assert_eq!(
        config.rank.weights,
        std::collections::HashMap::from([("kind".to_string(), 1.0f32)])
    );

    // Settings derived exactly as the router derives them from a default
    // config take the legacy path and reproduce its output.
    let settings = RankSettings {
        use_pipeline: config.rank.enabled,
        weights: WeightTable::from_config(&config.rank.weights).unwrap(),
    };

    let (dir, conn) = setup_indexed_corpus();
    let root = dir.path();
    for &query in QUERIES {
        let found = candidates(root, query);
        assert_equivalent(&found, Some(&conn), query, &settings);
        assert_equivalent(&found, None, query, &settings);
    }
}

// ---------------------------------------------------------------------------
// 6. End-to-end CLI byte identity (AR-033, the --why gate)
// ---------------------------------------------------------------------------

#[test]
fn cli_why_stdout_is_byte_identical_to_default_smart_run() {
    let (dir, _conn) = setup_indexed_corpus();
    let root = dir.path();
    // Isolated $HOME so the child processes cannot pick up a real global
    // config (the local index at <root>/.wonk/index.db needs no $HOME).
    let home = TempDir::new().unwrap();
    let bin = env!("CARGO_BIN_EXE_wonk");

    let run = |extra: &[&str]| {
        let mut cmd = std::process::Command::new(bin);
        cmd.current_dir(root)
            .env("HOME", home.path())
            .arg("search")
            .arg("cache")
            .arg("--smart");
        for arg in extra {
            cmd.arg(arg);
        }
        let output = cmd.output().expect("wonk binary to run");
        assert!(
            output.status.success(),
            "wonk search {extra:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    let plain = run(&[]);
    let explained = run(&["--why"]);

    assert!(
        !plain.stdout.is_empty(),
        "fixture corpus must produce search output"
    );
    // AR-033 at the CLI surface: stdout is byte-identical with --why.
    assert_eq!(
        plain.stdout, explained.stdout,
        "--why must not change stdout bytes"
    );

    let plain_err = String::from_utf8_lossy(&plain.stderr);
    let why_err = String::from_utf8_lossy(&explained.stderr);
    assert!(
        !plain_err.contains("why: "),
        "default run prints no why lines: {plain_err}"
    );
    assert!(
        why_err.contains("why: "),
        "--why prints per-result breakdown to stderr: {why_err}"
    );
    // Every why line carries the kind signal breakdown.
    for line in why_err.lines().filter(|l| l.starts_with("why: ")) {
        assert!(
            line.contains("kind "),
            "why line names the kind signal: {line}"
        );
        assert!(
            line.contains("total="),
            "why line shows the final score: {line}"
        );
    }
}

// ---------------------------------------------------------------------------
// 7. --why without --smart implies smart ranked mode
// ---------------------------------------------------------------------------

#[test]
fn cli_why_alone_implies_smart_ranked_mode() {
    // `--why` implies smart ranked mode via the router's
    // detect_search_mode(raw, smart || why, symbol_count). The pattern must
    // be TEXT-ONLY (no symbol match), since a symbol-matching pattern would
    // select Smart mode on its own and the implication would go unpinned.
    let (dir, conn) = setup_indexed_corpus();
    assert_eq!(
        db::count_matching_symbols(&conn, "eviction"),
        0,
        "'eviction' must stay a text-only pattern for this pin to hold"
    );
    let root = dir.path();
    let home = TempDir::new().unwrap();
    let bin = env!("CARGO_BIN_EXE_wonk");

    let run = |args: &[&str]| {
        let mut cmd = std::process::Command::new(bin);
        cmd.current_dir(root)
            .env("HOME", home.path())
            .arg("search")
            .arg("eviction");
        for arg in args {
            cmd.arg(arg);
        }
        let output = cmd.output().expect("wonk binary to run");
        assert!(
            output.status.success(),
            "wonk search {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    let plain = run(&[]);
    let smart = run(&["--smart"]);
    let why_alone = run(&["--why"]);

    assert!(
        !smart.stdout.is_empty(),
        "fixture corpus must produce search output"
    );
    // Without flags the text-only pattern takes Plain mode, so the two
    // modes are observably different outputs (anti-vacuity).
    assert_ne!(
        plain.stdout, smart.stdout,
        "plain and smart runs must differ for this pin to be meaningful"
    );
    assert_eq!(
        smart.stdout, why_alone.stdout,
        "--why alone must route through the same ranked pipeline as --smart"
    );

    let why_err = String::from_utf8_lossy(&why_alone.stderr);
    let why_lines: Vec<&str> = why_err.lines().filter(|l| l.starts_with("why: ")).collect();
    assert!(
        !why_lines.is_empty(),
        "--why alone must print breakdown lines to stderr: {why_err}"
    );
    for line in why_lines {
        assert!(
            line.contains("kind "),
            "why line names the kind signal: {line}"
        );
        assert!(
            line.contains("total="),
            "why line shows the final score: {line}"
        );
    }
}
