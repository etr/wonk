//! Ranking regression suite for BM25 re-ranking (TASK-079, AR-024).
//!
//! Fully offline and deterministic: the committed fixture corpus at
//! `tests/fixtures/bm25_corpus` (8 topic clusters, adversarial-alphabetical
//! peripheral paths) is copied into a temp dir, indexed with the real
//! pipeline, and queried through the real `text_search` path. The query set
//! and the expected top-10 lists are frozen so any scoring change that
//! shifts lexical ranking shows up as a diff.
//!
//! The BM25 path never consults learned feedback state (TASK-103,
//! PRD-FB-REQ-018): this suite is feedback-free by construction — no
//! `RankSettings` here loads a learned table, so accumulated feedback
//! cannot confirm itself through the frozen goldens.

mod common;

use std::fs;
use std::path::Path;

use rusqlite::Connection;
use tempfile::TempDir;
use wonk::bm25::{self, Bm25Params};
use wonk::db;
use wonk::ranker;
use wonk::search::{self, SearchResult};

const FIXTURE: &str = "tests/fixtures/bm25_corpus";
const PARAMS: Bm25Params = Bm25Params { k1: 1.2, b: 0.75 };

/// The fixed query set: 5 single-term + 3 multi-term.
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

/// Relevance labels: the query's topic cluster, i.e. its three central
/// files (for multi-term queries, the leading term's cluster).
fn relevant_for(query: &str) -> [&'static str; 3] {
    match query {
        "cache" | "cache eviction" => [
            "src/cache/mod.rs",
            "src/cache/engine.rs",
            "src/cache/store.rs",
        ],
        "retry" | "retry backoff" => [
            "src/retry/mod.rs",
            "src/retry/engine.rs",
            "src/retry/store.rs",
        ],
        "parser" | "parser token" => [
            "src/parser/mod.rs",
            "src/parser/engine.rs",
            "src/parser/store.rs",
        ],
        "token" => [
            "src/token/mod.rs",
            "src/token/engine.rs",
            "src/token/store.rs",
        ],
        "queue" => [
            "src/queue/mod.rs",
            "src/queue/engine.rs",
            "src/queue/store.rs",
        ],
        _ => panic!("unknown query: {query}"),
    }
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

/// Copy the fixture corpus into a temp dir, index it, and open the index.
fn setup_indexed_corpus() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    copy_dir(Path::new(FIXTURE), root);
    fs::create_dir(root.join(".git")).unwrap();
    common::build_index(root, true).unwrap();
    let index_path = db::find_existing_index(root).expect("fixture index to exist");
    let conn = db::open(&index_path).unwrap();
    (dir, conn)
}

/// Literal text search over the corpus, with root prefixes stripped so the
/// paths match the walker-relative `term_stats.file` keys (the same
/// convention the router produces when cwd is the repo root).
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

/// The V4 baseline: match-presence ranking, deterministically ordered by
/// `(file, line)`.
fn baseline_sorted(mut results: Vec<SearchResult>) -> Vec<SearchResult> {
    results.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));
    results
}

fn precision_at_10(results: &[SearchResult], relevant: &[&str]) -> f32 {
    results
        .iter()
        .take(10)
        .filter(|r| relevant.contains(&r.file.to_str().unwrap_or("")))
        .count() as f32
        / 10.0
}

fn top10(results: &[SearchResult]) -> Vec<(String, u64)> {
    results
        .iter()
        .take(10)
        .map(|r| (r.file.to_string_lossy().into_owned(), r.line))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn bm25_beats_match_presence_precision_at_10() {
    let (dir, conn) = setup_indexed_corpus();
    let root = dir.path();

    let mut bm25_mean = 0.0f32;
    let mut baseline_mean = 0.0f32;
    for &query in QUERIES {
        let found = candidates(root, query);
        assert!(!found.is_empty(), "query {query} must have candidates");
        let relevant = relevant_for(query);

        let baseline = baseline_sorted(found.clone());
        let ranked = bm25::rerank_lexical(&conn, &found, query, PARAMS)
            .unwrap_or_else(|| panic!("query {query}: stats present, expected rerank"));

        let p_bm25 = precision_at_10(&ranked, &relevant);
        let p_base = precision_at_10(&baseline, &relevant);
        assert!(
            p_bm25 >= p_base,
            "query {query}: bm25 p@10 {p_bm25} regressed below baseline {p_base}"
        );
        bm25_mean += p_bm25;
        baseline_mean += p_base;
    }
    bm25_mean /= QUERIES.len() as f32;
    baseline_mean /= QUERIES.len() as f32;
    assert!(
        bm25_mean > baseline_mean,
        "mean p@10 must strictly improve: bm25 {bm25_mean} vs baseline {baseline_mean}"
    );
}

/// Frozen golden top-10 lists, hand-reviewed against the corpus design
/// after the first verified run: every slot is a central cluster file
/// (peripherals, tf=2 across 251 lines, never surface), cross-file order
/// follows the (tf, length) profile confirmed via SQL against the built
/// index, and lines ascend within a file. A scoring change that shifts
/// lexical ranking shows up here as a deliberate diff.
const GOLDEN_TOP10: &[(&str, &[(&str, u64)])] = &[
    (
        "cache",
        &[
            ("src/cache/mod.rs", 9),
            ("src/cache/mod.rs", 17),
            ("src/cache/mod.rs", 18),
            ("src/cache/mod.rs", 26),
            ("src/cache/mod.rs", 27),
            ("src/cache/mod.rs", 35),
            ("src/cache/mod.rs", 36),
            ("src/cache/mod.rs", 41),
            ("src/cache/mod.rs", 42),
            ("src/cache/mod.rs", 43),
        ],
    ),
    (
        "retry",
        &[
            ("src/retry/store.rs", 9),
            ("src/retry/store.rs", 17),
            ("src/retry/store.rs", 19),
            ("src/retry/store.rs", 27),
            ("src/retry/store.rs", 29),
            ("src/retry/store.rs", 37),
            ("src/retry/store.rs", 38),
            ("src/retry/store.rs", 43),
            ("src/retry/store.rs", 44),
            ("src/retry/store.rs", 45),
        ],
    ),
    (
        "parser",
        &[
            ("src/parser/engine.rs", 9),
            ("src/parser/engine.rs", 17),
            ("src/parser/engine.rs", 18),
            ("src/parser/engine.rs", 26),
            ("src/parser/engine.rs", 27),
            ("src/parser/engine.rs", 35),
            ("src/parser/engine.rs", 36),
            ("src/parser/engine.rs", 40),
            ("src/parser/engine.rs", 41),
            ("src/parser/engine.rs", 42),
        ],
    ),
    (
        "token",
        &[
            ("src/token/mod.rs", 9),
            ("src/token/mod.rs", 17),
            ("src/token/mod.rs", 19),
            ("src/token/mod.rs", 27),
            ("src/token/mod.rs", 29),
            ("src/token/mod.rs", 37),
            ("src/token/mod.rs", 39),
            ("src/token/mod.rs", 44),
            ("src/token/mod.rs", 46),
            ("src/token/mod.rs", 47),
        ],
    ),
    (
        "queue",
        &[
            ("src/queue/store.rs", 9),
            ("src/queue/store.rs", 17),
            ("src/queue/store.rs", 18),
            ("src/queue/store.rs", 26),
            ("src/queue/store.rs", 27),
            ("src/queue/store.rs", 35),
            ("src/queue/store.rs", 36),
            ("src/queue/store.rs", 41),
            ("src/queue/store.rs", 42),
            ("src/queue/store.rs", 43),
        ],
    ),
    (
        "cache eviction",
        &[
            ("src/cache/mod.rs", 17),
            ("src/cache/mod.rs", 26),
            ("src/cache/mod.rs", 35),
            ("src/cache/mod.rs", 41),
            ("src/cache/mod.rs", 43),
            ("src/cache/mod.rs", 45),
            ("src/cache/engine.rs", 17),
            ("src/cache/engine.rs", 27),
            ("src/cache/engine.rs", 37),
            ("src/cache/engine.rs", 43),
        ],
    ),
    (
        "retry backoff",
        &[
            ("src/retry/mod.rs", 17),
            ("src/retry/mod.rs", 27),
            ("src/retry/mod.rs", 37),
            ("src/retry/mod.rs", 44),
            ("src/retry/mod.rs", 47),
            ("src/retry/mod.rs", 50),
            ("src/retry/engine.rs", 17),
            ("src/retry/engine.rs", 27),
            ("src/retry/engine.rs", 37),
            ("src/retry/engine.rs", 43),
        ],
    ),
    (
        "parser token",
        &[
            ("src/parser/engine.rs", 17),
            ("src/parser/engine.rs", 26),
            ("src/parser/engine.rs", 35),
            ("src/parser/engine.rs", 40),
            ("src/parser/engine.rs", 42),
            ("src/parser/mod.rs", 17),
            ("src/parser/mod.rs", 27),
            ("src/parser/mod.rs", 37),
            ("src/parser/mod.rs", 44),
            ("src/parser/mod.rs", 47),
        ],
    ),
];

#[test]
fn bm25_golden_top10_per_query() {
    let (dir, conn) = setup_indexed_corpus();
    let root = dir.path();

    for &(query, expected) in GOLDEN_TOP10 {
        let found = candidates(root, query);
        let ranked = bm25::rerank_lexical(&conn, &found, query, PARAMS).unwrap();
        let actual = top10(&ranked);
        let expected: Vec<(String, u64)> = expected
            .iter()
            .map(|(file, line)| (file.to_string(), *line))
            .collect();
        assert_eq!(actual, expected, "golden top-10 for query {query}");
    }
}

#[test]
fn bm25_candidates_are_permutation() {
    let (dir, conn) = setup_indexed_corpus();
    let root = dir.path();

    for &query in QUERIES {
        let found = candidates(root, query);
        let ranked = bm25::rerank_lexical(&conn, &found, query, PARAMS).unwrap();
        assert_eq!(ranked.len(), found.len(), "query {query}: length changed");

        let key = |r: &SearchResult| (r.file.to_string_lossy().into_owned(), r.line);
        let mut before: Vec<_> = found.iter().map(key).collect();
        let mut after: Vec<_> = ranked.iter().map(key).collect();
        before.sort();
        after.sort();
        assert_eq!(before, after, "query {query}: not a permutation");
    }
}

#[test]
fn pre_v5_index_falls_back_without_error() {
    let (dir, conn) = setup_indexed_corpus();
    let root = dir.path();

    assert!(db::bm25_generation_ready(&conn));
    // A legacy index has no complete-generation marker. Empty term stats alone
    // also describe a valid, fully indexed zero-term corpus.
    conn.execute("DELETE FROM term_stats", []).unwrap();
    conn.execute("DELETE FROM bm25_meta WHERE key = 'generation_ready'", [])
        .unwrap();
    assert!(!db::bm25_generation_ready(&conn));

    let found = candidates(root, "cache");
    assert!(
        bm25::rerank_lexical(&conn, &found, "cache", PARAMS).is_none(),
        "pre-V5 index must signal the V4 fallback"
    );

    // The untouched list still fuses cleanly — the V5 behavior on fallback.
    let fused = ranker::fuse_rrf(&found, &[], 60.0);
    assert_eq!(fused.len(), found.len());
    let mut before: Vec<_> = found
        .iter()
        .map(|r| (r.file.to_string_lossy().into_owned(), r.line))
        .collect();
    let mut after: Vec<_> = fused.iter().map(|r| (r.file.clone(), r.line)).collect();
    before.sort();
    after.sort();
    assert_eq!(
        after, before,
        "fallback fusion must preserve every candidate"
    );
}
