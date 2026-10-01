//! TASK-094 acceptance tests: the path-character, proximity, and
//! signature signals.
//!
//! Library-level, over hand-seeded index databases (the TASK-092/093
//! matrix pattern) and explicit `WeightTable`s. Each test pins one
//! acceptance criterion of the task, including the discriminating case
//! the legacy ordinal ranking cannot produce.

use std::collections::HashMap;

use rusqlite::Connection;
use tempfile::TempDir;
use wonk::ranker::{self, ResultCategory};
use wonk::rerank::{self, ContextSources, QueryInfo, RankSettings, WeightTable};
use wonk::search::SearchResult;

fn hit(file: &str, line: u64, content: &str) -> SearchResult {
    SearchResult {
        file: std::path::PathBuf::from(file),
        line,
        col: 1,
        content: content.to_string(),
    }
}

fn table(entries: &[(&str, f32)]) -> WeightTable {
    WeightTable::from_pairs(entries.iter().map(|(n, w)| (n.to_string(), *w))).unwrap()
}

fn sources() -> ContextSources {
    ContextSources::default()
}

fn position(scored: &rerank::ScoredResult) -> (String, u64) {
    (
        scored.classified.result.file.to_string_lossy().into_owned(),
        scored.classified.result.line,
    )
}

fn positions(scored: &[rerank::ScoredResult]) -> Vec<(String, u64)> {
    scored.iter().map(position).collect()
}

fn contribution(scored: &rerank::ScoredResult, signal: &str) -> f32 {
    scored
        .contributions
        .iter()
        .find(|c| c.signal == signal)
        .map(|c| c.value)
        .unwrap_or(f32::NAN)
}

fn insert_file(conn: &Connection, path: &str, line_count: i64) {
    conn.execute(
        "INSERT INTO files (path, language, hash, last_indexed, line_count) \
         VALUES (?1, 'rust', 'h', 0, ?2)",
        rusqlite::params![path, line_count],
    )
    .unwrap();
}

fn insert_term(conn: &Connection, term: &str, file: &str, tf: i64) {
    conn.execute(
        "INSERT INTO term_stats (term, file, tf) VALUES (?1, ?2, ?3)",
        rusqlite::params![term, file, tf],
    )
    .unwrap();
}

fn insert_symbol(conn: &Connection, name: &str, file: &str, line: i64) {
    conn.execute(
        "INSERT INTO symbols (name, kind, file, line, col, language) \
         VALUES (?1, 'function', ?2, ?3, 0, 'rust')",
        rusqlite::params![name, file, line],
    )
    .unwrap();
}

fn open_conn() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let conn = wonk::db::open(&dir.path().join("index.db")).unwrap();
    (dir, conn)
}

fn pipeline_settings(weights: &WeightTable) -> RankSettings {
    RankSettings {
        use_pipeline: true,
        weights: weights.clone(),
        sources: sources(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// AC 1: test-file and implementation results with equal lexical scores
// rank implementation first
// ---------------------------------------------------------------------------

/// `gamma` defined in an ordinary file and in a test file; identical term
/// statistics (tf 3, 100 lines each) so the lexical range is degenerate and
/// the lexical signal contributes exactly 0 to both.
fn equal_lexical_conn() -> (TempDir, Connection) {
    let (dir, conn) = open_conn();
    insert_file(&conn, "src/impl.rs", 100);
    insert_file(&conn, "tests/gamma_test.rs", 100);
    insert_term(&conn, "gamma", "src/impl.rs", 3);
    insert_term(&conn, "gamma", "tests/gamma_test.rs", 3);
    insert_symbol(&conn, "gamma", "src/impl.rs", 5);
    insert_symbol(&conn, "gamma", "tests/gamma_test.rs", 3);
    (dir, conn)
}

fn equal_lexical_candidates() -> Vec<SearchResult> {
    vec![
        hit("src/impl.rs", 5, "pub fn gamma() {}"),
        hit("tests/gamma_test.rs", 3, "pub fn gamma() {}"),
    ]
}

#[test]
fn equal_lexical_scores_rank_implementation_first() {
    let (_dir, conn) = equal_lexical_conn();
    let found = equal_lexical_candidates();

    // Premise: identical tf and length make the lexical range degenerate —
    // both lexical contributions are exactly 0, so lexical cannot decide.
    let classified = ranker::classify_results(&found, Some(&conn));
    assert_eq!(classified[0].category, ResultCategory::Definition);
    assert_eq!(classified[1].category, ResultCategory::Test);

    // Path signal alone (kind = 0): the demotion survives without the kind
    // signal — 1.0 ordinary against 0.20 test.
    let path_only = rerank::rerank(
        ranker::classify_results(&found, Some(&conn)),
        &QueryInfo { pattern: "gamma" },
        Some(&conn),
        &table(&[("path_character", 1.0)]),
        &sources(),
    );
    assert_eq!(contribution(&path_only[0], "path_character"), 1.0);
    assert_eq!(contribution(&path_only[1], "path_character"), 0.20);
    assert_eq!(
        positions(&path_only),
        vec![
            ("src/impl.rs".to_string(), 5),
            ("tests/gamma_test.rs".to_string(), 3),
        ],
        "implementation must rank first under path_character alone"
    );

    // Combined stack (kind + lexical + path): equal lexical scores (both
    // exactly 0 in the breakdown) leave the demotion decisive.
    let combined = rerank::rerank(
        ranker::classify_results(&found, Some(&conn)),
        &QueryInfo { pattern: "gamma" },
        Some(&conn),
        &table(&[("kind", 1.0), ("lexical", 1.0), ("path_character", 1.0)]),
        &sources(),
    );
    assert_eq!(contribution(&combined[0], "lexical"), 0.0);
    assert_eq!(contribution(&combined[1], "lexical"), 0.0);
    assert_eq!(
        positions(&combined),
        vec![
            ("src/impl.rs".to_string(), 5),
            ("tests/gamma_test.rs".to_string(), 3),
        ],
        "implementation must rank first under the combined stack"
    );
}

// ---------------------------------------------------------------------------
// AC 2: a generated file is demoted only when a hand-written same-named
// peer exists
// ---------------------------------------------------------------------------

#[test]
fn generated_file_demoted_only_with_verified_peer() {
    // The peer is NOT a query candidate in either run — only the index's
    // files table differs between them.
    let found = vec![
        hit("src/foo.g.dart", 2, "class Foo {}"),
        hit("src/other.rs", 1, "pub fn gamma() {}"),
    ];
    let query = QueryInfo { pattern: "gamma" };
    let weights = table(&[("path_character", 1.0)]);

    // Without the peer in the index: Ordinary, never demoted (ties with
    // other.rs at 1.0; the (file, line) tie-break keeps the stable order).
    let (dir_no_peer, conn_no_peer) = open_conn();
    insert_file(&conn_no_peer, "src/foo.g.dart", 10);
    let no_peer = rerank::rerank(
        ranker::classify_results(&found, None),
        &query,
        Some(&conn_no_peer),
        &weights,
        &sources(),
    );
    assert_eq!(contribution(&no_peer[0], "path_character"), 1.0);
    assert_eq!(contribution(&no_peer[1], "path_character"), 1.0);
    assert_eq!(
        positions(&no_peer)[0],
        ("src/foo.g.dart".to_string(), 2),
        "peerless generated file is not demoted"
    );
    drop(dir_no_peer);

    // With the peer indexed: GeneratedShadowed 0.10, below the ordinary
    // file.
    let (dir_peer, conn_peer) = open_conn();
    insert_file(&conn_peer, "src/foo.g.dart", 10);
    insert_file(&conn_peer, "src/foo.dart", 80);
    let with_peer = rerank::rerank(
        ranker::classify_results(&found, None),
        &query,
        Some(&conn_peer),
        &weights,
        &sources(),
    );
    assert_eq!(
        contribution(&with_peer[0], "path_character"),
        1.0,
        "the hand-written file keeps the ordinary value"
    );
    assert_eq!(contribution(&with_peer[1], "path_character"), 0.10);
    assert_eq!(
        positions(&with_peer),
        vec![
            ("src/other.rs".to_string(), 1),
            ("src/foo.g.dart".to_string(), 2),
        ],
        "shadowed generated file demotes below ordinary files"
    );
    drop(dir_peer);
}

// ---------------------------------------------------------------------------
// AC 3: a symbol that exists only in a test still returns; graded, never
// exclusion
// ---------------------------------------------------------------------------

#[test]
fn test_only_symbol_still_returned_under_kind_and_path() {
    let (dir, conn) = open_conn();
    insert_symbol(&conn, "gamma", "tests/gamma_test.rs", 3);
    let found = vec![hit("tests/gamma_test.rs", 3, "pub fn gamma() {}")];

    let groups = rerank::rank_and_explain(
        &found,
        Some(&conn),
        "gamma",
        &pipeline_settings(&table(&[("kind", 1.0), ("path_character", 1.0)])),
    );
    let flat: Vec<(String, u64)> = groups
        .iter()
        .flat_map(|(_, items)| items.iter().map(position))
        .collect();
    assert_eq!(
        flat,
        vec![("tests/gamma_test.rs".to_string(), 3)],
        "a test-only symbol must still be returned"
    );
    // The reduction is graded, never exclusion: the test file's path
    // contribution is strictly positive.
    let scored = &groups[0].1[0];
    assert_eq!(contribution(scored, "path_character"), 0.20);
    assert!(contribution(scored, "path_character") > 0.0);
    drop(dir);
}

#[test]
fn test_file_survives_every_signal_enabled() {
    let (_dir, conn) = open_conn();
    insert_symbol(&conn, "gamma", "tests/gamma_test.rs", 3);
    let found = vec![hit("tests/gamma_test.rs", 3, "pub fn gamma() {}")];

    // Every signal at weight 1.0: the pipeline orders, it never filters.
    let weights = table(&[
        ("kind", 1.0),
        ("lexical", 1.0),
        ("semantic", 1.0),
        ("centrality", 1.0),
        ("prominence", 1.0),
        ("path_character", 1.0),
        ("proximity", 1.0),
        ("signature", 1.0),
    ]);
    let scored = rerank::rerank(
        ranker::classify_results(&found, Some(&conn)),
        &QueryInfo { pattern: "gamma" },
        Some(&conn),
        &weights,
        &sources(),
    );
    assert_eq!(scored.len(), 1, "graded reduction never excludes");
    assert!(contribution(&scored[0], "path_character") > 0.0);
}

// ---------------------------------------------------------------------------
// Proximity pin through rank_and_explain
// ---------------------------------------------------------------------------

#[test]
fn proximity_orders_tighter_co_location_first() {
    let found = vec![
        hit("src/far.rs", 1, "alpha filler1 filler2 beta;"),
        hit("src/near.rs", 1, "alpha beta;"),
    ];
    let groups = rerank::rank_and_explain(
        &found,
        None,
        "alpha beta",
        &pipeline_settings(&table(&[("proximity", 1.0)])),
    );
    let flat: Vec<(String, u64)> = groups
        .iter()
        .flat_map(|(_, items)| items.iter().map(position))
        .collect();
    assert_eq!(
        flat,
        vec![
            ("src/near.rs".to_string(), 1),
            ("src/far.rs".to_string(), 1),
        ],
        "adjacent query terms must outrank distant ones"
    );
}

// ---------------------------------------------------------------------------
// Signature pin through rank_and_explain
// ---------------------------------------------------------------------------

#[test]
fn signature_shaped_query_prefers_the_definition() {
    let (dir, conn) = open_conn();
    insert_symbol(&conn, "parse", "src/parse.rs", 3);
    let found = vec![
        hit("src/main.rs", 8, "parse(data);"),
        hit("src/parse.rs", 3, "fn parse(input: &str) -> Vec<Token> {"),
    ];

    let groups = rerank::rank_and_explain(
        &found,
        Some(&conn),
        "parse(input: &str)",
        &pipeline_settings(&table(&[("signature", 1.0)])),
    );
    let flat: Vec<(String, u64)> = groups
        .iter()
        .flat_map(|(_, items)| items.iter().map(position))
        .collect();
    assert_eq!(
        flat,
        vec![
            ("src/parse.rs".to_string(), 3),
            ("src/main.rs".to_string(), 8),
        ],
        "a signature-shaped query must rank the definition first"
    );

    // The same candidates under a NAME-shaped query: the signal is inert —
    // every signature contribution is exactly 0 and every score is 0 (the
    // emitted order is just the category bucketing, not this signal).
    let plain = rerank::rank_and_explain(
        &found,
        Some(&conn),
        "parse",
        &pipeline_settings(&table(&[("signature", 1.0)])),
    );
    let flat_plain: Vec<(String, u64)> = plain
        .iter()
        .flat_map(|(_, items)| items.iter().map(position))
        .collect();
    assert_eq!(
        flat_plain,
        vec![
            ("src/parse.rs".to_string(), 3),
            ("src/main.rs".to_string(), 8),
        ],
        "name-shaped queries leave the signature signal inert"
    );
    for (_, items) in &plain {
        for scored in items {
            assert_eq!(contribution(scored, "signature"), 0.0);
            assert_eq!(scored.score, 0.0);
        }
    }
    drop(dir);
}

// ---------------------------------------------------------------------------
// Cost contract: kind-only weights still prepare no context (REQ-003)
// ---------------------------------------------------------------------------

#[test]
fn default_weights_run_no_new_context_paths() {
    // The default [rank.weights] table over a fully seeded connection: the
    // union of active requirements is empty, so the three new signals cost
    // nothing — the equivalence contract from TASK-092/093 holds.
    let (_dir, conn) = open_conn();
    insert_file(&conn, "a.rs", 100);
    insert_term(&conn, "alpha", "a.rs", 5);
    insert_symbol(&conn, "alpha", "a.rs", 1);
    let weights = WeightTable::from_config(&HashMap::from([("kind".to_string(), 1.0f32)])).unwrap();

    assert_eq!(
        rerank::builtin_signals().len(),
        13,
        "registry carries the TASK-093/094 signals plus churn, co_change, hub, authority and \
         community"
    );
    let scored = rerank::rerank(
        ranker::classify_results(&[hit("a.rs", 1, "alpha")], Some(&conn)),
        &QueryInfo { pattern: "alpha" },
        Some(&conn),
        &weights,
        &sources(),
    );
    for s in &scored {
        assert_eq!(s.contributions.len(), 1);
        assert_eq!(s.contributions[0].signal, "kind");
    }
}
