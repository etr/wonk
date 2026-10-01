//! TASK-093 acceptance tests: the structural and lexical signals.
//!
//! Library-level, over hand-seeded index databases (the TASK-092 matrix
//! pattern extended with `files`/`term_stats`/`embeddings` rows) and
//! explicit `WeightTable`s. Each test pins one acceptance criterion of the
//! task, including the discriminating case that ordinal (kind-only)
//! ranking cannot produce.

use std::collections::HashMap;

use rusqlite::Connection;
use tempfile::TempDir;
use wonk::ranker::{self, ResultCategory};
use wonk::rerank::{self, ContextSources, QueryInfo, WeightTable};
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

// -- shared seeding helpers --------------------------------------------------

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

fn insert_symbol(conn: &Connection, name: &str, file: &str, line: i64) -> i64 {
    conn.execute(
        "INSERT INTO symbols (name, kind, file, line, col, language) \
         VALUES (?1, 'function', ?2, ?3, 0, 'rust')",
        rusqlite::params![name, file, line],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn insert_reference(conn: &Connection, name: &str, file: &str, line: i64, caller: Option<i64>) {
    conn.execute(
        "INSERT INTO \"references\" (name, file, line, col, context, caller_id) \
         VALUES (?1, ?2, ?3, 0, ?4, ?5)",
        rusqlite::params![name, file, line, format!("{name}();"), caller],
    )
    .unwrap();
}

fn open_conn() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let conn = wonk::db::open(&dir.path().join("index.db")).unwrap();
    (dir, conn)
}

// ---------------------------------------------------------------------------
// AC 1: a strong call-site match outranks a weak definition
// ---------------------------------------------------------------------------

/// `gamma` defined once in a huge file (tf 1, 2000 lines) but hammered in a
/// tiny one (tf 8, 50 lines).
fn strong_call_site_conn() -> (TempDir, Connection) {
    let (dir, conn) = open_conn();
    insert_file(&conn, "def.rs", 2000);
    insert_file(&conn, "call.rs", 50);
    insert_term(&conn, "gamma", "def.rs", 1);
    insert_term(&conn, "gamma", "call.rs", 8);
    insert_symbol(&conn, "gamma", "def.rs", 10);
    insert_reference(&conn, "gamma", "call.rs", 5, None);
    (dir, conn)
}

#[test]
fn strong_call_site_outranks_weak_definition() {
    let (_dir, conn) = strong_call_site_conn();
    let found = vec![
        hit("def.rs", 10, "fn gamma() {}"),
        hit("call.rs", 5, "gamma(3);"),
    ];

    // The impossible-under-ordinal proof: the legacy kind-only ranker puts
    // the definition tier first no matter how weak the match is.
    let classified = ranker::classify_results(&found, Some(&conn));
    assert_eq!(classified[0].category, ResultCategory::Definition);
    assert_eq!(classified[1].category, ResultCategory::CallSite);
    let legacy = ranker::rank_results(ranker::classify_results(&found, Some(&conn)));
    assert_eq!(legacy[0].result.file, std::path::Path::new("def.rs"));

    let scored = rerank::rerank(
        classified,
        &QueryInfo { pattern: "gamma" },
        Some(&conn),
        &table(&[("kind", 1.0), ("lexical", 1.0)]),
        &sources(),
    );

    // The call site now wins: 0.8 kind + 1.0 lexical against 1.0 + 0.0.
    assert_eq!(
        positions(&scored),
        vec![("call.rs".to_string(), 5), ("def.rs".to_string(), 10)]
    );
    assert!((scored[0].score - 1.8).abs() < 1e-6, "{}", scored[0].score);
    assert!((scored[1].score - 1.0).abs() < 1e-6, "{}", scored[1].score);
    // The breakdown retains the UNWEIGHTED lexical values.
    assert_eq!(contribution(&scored[0], "lexical"), 1.0);
    assert_eq!(contribution(&scored[1], "lexical"), 0.0);
}

// ---------------------------------------------------------------------------
// AC 2: caller-count damping prevents hub dominance
// ---------------------------------------------------------------------------

/// Three definitions of unrelated names all matching query term `gamma`:
/// `mid_thing` in a tiny dense file with 5 distinct callers, `hub_thing`
/// (the 500-caller hub) in a huge sparse file, and a long filler with no
/// callers that anchors the lexical scale's minimum.
fn hub_conn() -> (TempDir, Connection) {
    let (dir, conn) = open_conn();
    insert_file(&conn, "mid.rs", 50);
    insert_file(&conn, "hub.rs", 2000);
    insert_file(&conn, "filler.rs", 5000);
    insert_term(&conn, "gamma", "mid.rs", 20);
    insert_term(&conn, "gamma", "hub.rs", 1);
    insert_term(&conn, "gamma", "filler.rs", 1);
    insert_symbol(&conn, "mid_thing", "mid.rs", 10);
    insert_symbol(&conn, "hub_thing", "hub.rs", 10);
    insert_symbol(&conn, "filler_thing", "filler.rs", 10);

    // Foreign keys are ON: every distinct caller_id needs a symbols row.
    // The three definitions above take autoincrement ids 1-3, so caller
    // symbols are inserted with explicit ids starting at 1000.
    let mut symbols = String::new();
    let mut references = String::new();
    for (name, file, callers, base) in [
        ("mid_thing", "mid.rs", 5usize, 1000i64),
        ("hub_thing", "hub.rs", 500, 2000),
    ] {
        for i in 0..callers {
            let caller_id = base + i as i64;
            symbols.push_str(&format!(
                "INSERT INTO symbols (id, name, kind, file, line, col, language) \
                 VALUES ({caller_id}, 'caller_{caller_id}', 'function', 'callers.rs', 1, 0, 'rust');\n"
            ));
            let reference_line = i as i64 + 20;
            references.push_str(&format!(
                "INSERT INTO \"references\" (name, file, line, col, context, caller_id) \
                 VALUES ('{name}', '{file}', {reference_line}, 0, '{name}();', {caller_id});\n"
            ));
        }
    }
    conn.execute_batch(&symbols).unwrap();
    conn.execute_batch(&references).unwrap();
    (dir, conn)
}

#[test]
fn hub_centrality_does_not_dominate_unrelated_query() {
    let (_dir, conn) = hub_conn();
    let found = vec![
        hit("mid.rs", 10, "fn mid_thing() {}"),
        hit("hub.rs", 10, "fn hub_thing() {}"),
        hit("filler.rs", 10, "fn filler_thing() {}"),
    ];

    let scored = rerank::rerank(
        ranker::classify_results(&found, Some(&conn)),
        &QueryInfo { pattern: "gamma" },
        Some(&conn),
        &table(&[("lexical", 1.0), ("centrality", 1.0)]),
        &sources(),
    );

    // The fixture is the discriminating case: the hub's normalized lexical
    // score lands strictly between the linear (0.010) and the log-damped
    // (0.288) centrality of 5-of-500 callers, so only the damping decides
    // the winner.
    let hub_lex = contribution(&scored[1], "lexical");
    let log_centrality = rerank::centrality_value(5, 500);
    let linear_centrality = 5.0f32 / 500.0f32;
    assert!(
        hub_lex > linear_centrality && hub_lex < log_centrality,
        "fixture must discriminate log from linear: hub_lex={hub_lex}"
    );

    // Log-damped centrality: the mid-tier definition wins.
    assert_eq!(positions(&scored)[0], ("mid.rs".to_string(), 10));
    assert!(
        scored[0].score > scored[1].score,
        "mid must outrank the hub: {} vs {}",
        scored[0].score,
        scored[1].score
    );
    // Counterfactual (arithmetic on the observed values): under a LINEAR
    // caller ratio the hub would win — 1.0 + hub_lex against
    // 1.0 + 0.010.
    assert!(
        hub_lex + 1.0 > 1.0 + linear_centrality,
        "under linear centrality the hub would dominate"
    );
    // The un-called filler stays at exactly zero on both signals.
    assert_eq!(contribution(&scored[2], "centrality"), 0.0);
}

// ---------------------------------------------------------------------------
// AC 3: missing embeddings contribute zero, not a penalty
// ---------------------------------------------------------------------------

/// `gamma` defined at def.rs:10 (optionally embedded), called at call.rs:5,
/// mentioned in a comment at note.rs:2. The stored candidate vector is the
/// bundled provider's own embedding of the query, so the similarity is
/// deterministic and exact.
fn semantic_conn(with_embedding: bool) -> (TempDir, Connection) {
    let (dir, conn) = open_conn();
    let symbol_id = insert_symbol(&conn, "gamma", "def.rs", 10);
    insert_reference(&conn, "gamma", "call.rs", 5, None);
    if with_embedding {
        let provider =
            wonk::embedding::create_provider(wonk::embedding::EmbeddingProviderKind::Bundled)
                .unwrap();
        let mut vector = provider.embed_single("gamma").unwrap();
        wonk::embedding::normalize(&mut vector);
        conn.execute(
            "INSERT INTO embeddings \
             (symbol_id, file, chunk_text, vector, stale, created_at, provider, dim) \
             VALUES (?1, 'def.rs', 'gamma chunk', ?2, 0, 0, 'bundled', 256)",
            rusqlite::params![symbol_id, bytemuck::cast_slice(&vector)],
        )
        .unwrap();
    }
    (dir, conn)
}

fn semantic_candidates() -> Vec<SearchResult> {
    vec![
        hit("def.rs", 10, "fn gamma() {}"),
        hit("call.rs", 5, "gamma(3);"),
        hit("note.rs", 2, "// gamma note"),
    ]
}

#[test]
fn missing_embeddings_contribute_zero_not_penalty() {
    let (_dir, conn) = semantic_conn(true);
    let classified = ranker::classify_results(&semantic_candidates(), Some(&conn));
    assert_eq!(classified[0].category, ResultCategory::Definition);
    assert_eq!(classified[1].category, ResultCategory::CallSite);
    assert_eq!(classified[2].category, ResultCategory::Comment);

    let kind_only = rerank::rerank(
        ranker::classify_results(&semantic_candidates(), Some(&conn)),
        &QueryInfo { pattern: "gamma" },
        Some(&conn),
        &table(&[("kind", 1.0)]),
        &sources(),
    );
    let scored = rerank::rerank(
        classified,
        &QueryInfo { pattern: "gamma" },
        Some(&conn),
        &table(&[("kind", 1.0), ("semantic", 1.0)]),
        &sources(),
    );

    // The embedded definition is (near-)perfectly similar to its own query.
    assert!(
        (contribution(&scored[0], "semantic") - 1.0).abs() < 1e-5,
        "{}",
        contribution(&scored[0], "semantic")
    );
    // Everyone else contributes EXACTLY zero — their kind score untouched.
    for scored in &scored[1..] {
        assert_eq!(contribution(scored, "semantic"), 0.0);
        assert_eq!(scored.score, rerank::kind_value(scored.classified.category));
    }
    // Zero-not-penalty: the kind-only order survives intact.
    assert_eq!(positions(&scored), positions(&kind_only));
}

// ---------------------------------------------------------------------------
// AC 4 (regression guard): no embeddings anywhere changes nothing
// ---------------------------------------------------------------------------

#[test]
fn no_embeddings_anywhere_leaves_order_unchanged() {
    let (_dir, conn) = semantic_conn(false);
    let query = QueryInfo { pattern: "gamma" };
    let kind_only = rerank::rerank(
        ranker::classify_results(&semantic_candidates(), Some(&conn)),
        &query,
        Some(&conn),
        &table(&[("kind", 1.0)]),
        &sources(),
    );
    let with_semantic = rerank::rerank(
        ranker::classify_results(&semantic_candidates(), Some(&conn)),
        &query,
        Some(&conn),
        &table(&[("kind", 1.0), ("semantic", 1.0)]),
        &sources(),
    );

    // Identical order AND identical scores: the absent signal contributed
    // exactly nothing.
    assert_eq!(positions(&with_semantic), positions(&kind_only));
    for (semantic, plain) in with_semantic.iter().zip(&kind_only) {
        assert_eq!(semantic.score, plain.score);
        assert_eq!(contribution(semantic, "semantic"), 0.0);
    }
}

// ---------------------------------------------------------------------------
// AC 5 (zero-cost gate): default weights never touch the new signals
// ---------------------------------------------------------------------------

#[test]
fn default_weights_skip_all_new_signals() {
    // A fully seeded index — term stats, symbols, references, embeddings —
    // so every supplementary source EXISTS and could have been consulted.
    let (dir, conn) = semantic_conn(true);
    insert_file(&conn, "def.rs", 100);
    insert_file(&conn, "call.rs", 100);
    insert_term(&conn, "gamma", "def.rs", 3);
    insert_term(&conn, "gamma", "call.rs", 3);

    // The default [rank.weights] table: kind only.
    let weights = WeightTable::from_config(&HashMap::from([("kind".to_string(), 1.0f32)])).unwrap();

    let scored = rerank::rerank(
        ranker::classify_results(&semantic_candidates(), Some(&conn)),
        &QueryInfo { pattern: "gamma" },
        Some(&conn),
        &weights,
        &sources(),
    );

    for scored in &scored {
        assert_eq!(scored.contributions.len(), 1, "only kind is evaluated");
        assert_eq!(scored.contributions[0].signal, "kind");
    }
    drop(dir);
}
