//! BM25 lexical scoring (TASK-079, DR-033). Re-ranks the grep candidate set
//! using index-time term statistics (TASK-078 `term_stats`) and exposes the
//! ranked list as the lexical input to RRF fusion. Re-ranks; never re-retrieves.
//!
//! The scoring variant is Lucene-style non-negative IDF,
//! `idf = ln(1 + (N - df + 0.5)/(df + 0.5))`: in a code corpus, terms like
//! `fn`/`pub` have df > N/2, where classic Robertson IDF goes negative.
//! Query terms are deduplicated (no qtf/k3 term). The score is per-file;
//! line results inherit their file's score, with ties broken by
//! `(file asc, line asc)` for a total deterministic order.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::config::SearchConfig;
use crate::search::SearchResult;

/// Tunable BM25 constants, sourced from `[search]` config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bm25Params {
    /// Term-frequency saturation strength.
    pub k1: f32,
    /// Length-normalization strength in `[0, 1]`.
    pub b: f32,
}

impl From<&SearchConfig> for Bm25Params {
    fn from(config: &SearchConfig) -> Self {
        Self {
            k1: config.bm25_k1,
            b: config.bm25_b,
        }
    }
}

/// Corpus-level statistics needed to normalize scores.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CorpusStats {
    /// Number of indexed documents (files).
    pub n_docs: u64,
    /// Mean document length in lines.
    pub avg_doc_len: f32,
}

/// Load `(COUNT(*), AVG(line_count))` from `files`.
///
/// Returns `None` for a degenerate corpus: no files at all, or no file
/// with a non-NULL `line_count`.
pub fn load_corpus_stats(conn: &Connection) -> Option<CorpusStats> {
    let (n_docs, avg): (i64, Option<f64>) = conn
        .query_row("SELECT COUNT(*), AVG(line_count) FROM files", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .ok()?;
    Some(CorpusStats {
        n_docs: n_docs as u64,
        avg_doc_len: avg? as f32,
    })
}

/// Lucene-style non-negative inverse document frequency.
pub fn idf(n_docs: u64, df: u64) -> f32 {
    (1.0 + (n_docs as f32 - df as f32 + 0.5) / (df as f32 + 0.5)).ln()
}

/// One query term's contribution to a document's score.
///
/// `idf * tf * (k1 + 1) / (tf + k1 * (1 - b + b * doc_len / avgdl))`.
/// A denominator that is not positive — pathological params, or a 0/0
/// length ratio on an all-zero corpus — contributes 0 rather than NaN.
#[allow(clippy::too_many_arguments)]
pub fn term_contribution(tf: u64, doc_len: f32, avgdl: f32, idf: f32, k1: f32, b: f32) -> f32 {
    let tf = tf as f32;
    let denominator = tf + k1 * (1.0 - b + b * doc_len / avgdl);
    if !(denominator > 0.0) {
        return 0.0;
    }
    idf * tf * (k1 + 1.0) / denominator
}

/// Re-rank grep candidates by BM25 over `term_stats`, or `None` when
/// scoring is unavailable (pre-V5 index, empty or dropped `term_stats`,
/// degenerate corpus, query error). `None` means "keep the input order" —
/// the V4 fallback — never an error.
///
/// Empty results or a query with no tokens return the input unchanged.
/// The output is always a permutation of the input: same set, reordered by
/// `(score desc, file asc, line asc)`; line results inherit their file's
/// score. Query cost is `2 + T` statements for T query terms.
pub fn rerank_lexical(
    conn: &Connection,
    results: &[SearchResult],
    query: &str,
    params: Bm25Params,
) -> Option<Vec<SearchResult>> {
    // Presence probe: an index built before TASK-078 (or a freshly opened
    // but unpopulated one) has no rows here — signal the V4 fallback.
    conn.query_row("SELECT 1 FROM term_stats LIMIT 1", [], |_| Ok(()))
        .ok()?;

    if results.is_empty() {
        return Some(results.to_vec());
    }
    let mut terms = crate::tokenizer::tokenize(query);
    terms.sort_unstable();
    terms.dedup();
    if terms.is_empty() {
        return Some(results.to_vec());
    }

    let corpus = load_corpus_stats(conn)?;

    // Postings per query term: tf by file. The postings map's size is df.
    let mut scored_terms: Vec<(f32, HashMap<String, u64>)> = Vec::new();
    for term in &terms {
        let mut stmt = conn
            .prepare("SELECT file, tf FROM term_stats WHERE term = ?1")
            .ok()?;
        let rows = stmt
            .query_map(rusqlite::params![term], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })
            .ok()?;
        let mut tf_by_file = HashMap::new();
        for row in rows {
            let (file, tf) = row.ok()?;
            tf_by_file.insert(file, tf);
        }
        if tf_by_file.is_empty() {
            continue; // df = 0: the term is absent from the corpus
        }
        let idf = idf(corpus.n_docs, tf_by_file.len() as u64);
        scored_terms.push((idf, tf_by_file));
    }

    // Document lengths; files missing from the index (or with NULL length)
    // fall back to avgdl, which is length-neutral.
    let mut doc_len: HashMap<String, f32> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT path, line_count FROM files").ok()?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
            })
            .ok()?;
        for row in rows {
            let (path, len) = row.ok()?;
            if let Some(len) = len {
                doc_len.insert(path, len as f32);
            }
        }
    }

    // Score each distinct candidate file.
    let mut scores: HashMap<String, f32> = HashMap::new();
    for result in results {
        let file = result.file.to_string_lossy().into_owned();
        if scores.contains_key(&file) {
            continue;
        }
        let len = doc_len.get(&file).copied().unwrap_or(corpus.avg_doc_len);
        let score = scored_terms
            .iter()
            .map(|(idf, tf_by_file)| {
                let tf = tf_by_file.get(&file).copied().unwrap_or(0);
                term_contribution(tf, len, corpus.avg_doc_len, *idf, params.k1, params.b)
            })
            .sum();
        scores.insert(file, score);
    }

    let mut ranked: Vec<(f32, SearchResult)> = results
        .to_vec()
        .into_iter()
        .map(|r| {
            (
                scores
                    .get(r.file.to_string_lossy().as_ref())
                    .copied()
                    .unwrap_or(0.0),
                r,
            )
        })
        .collect();
    ranked.sort_by(|(score_a, a), (score_b, b)| {
        score_b
            .total_cmp(score_a)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    Some(ranked.into_iter().map(|(_, r)| r).collect())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    const TOL: f32 = 1e-4;

    #[test]
    fn idf_rare_term() {
        // ln(1 + 9.5/1.5) = ln(7.3333...)
        assert!((idf(10, 1) - 1.99243).abs() < TOL);
    }

    #[test]
    fn idf_decreases_with_df() {
        let rare = idf(100, 1);
        let common = idf(100, 50);
        let universal = idf(100, 100);
        assert!(rare > common, "rare terms must outweigh common ones");
        assert!(common > universal);
    }

    #[test]
    fn idf_never_negative() {
        // Even a term present in every document stays (barely) positive:
        // ln(1 + 0.5/10.5) = ln(1.047619)
        assert!((idf(10, 10) - 0.04652).abs() < TOL);
        assert!(idf(10, 10) >= 0.0);
    }

    #[test]
    fn term_contribution_hand_computed() {
        // tf=2, len=avgdl=100, k1=1.2, b=0.75, idf=1:
        // 2*(1.2+1) / (2 + 1.2*(0.25 + 0.75*1)) = 4.4/3.2 = 1.375
        assert!((term_contribution(2, 100.0, 100.0, 1.0, 1.2, 0.75) - 1.375).abs() < TOL);
    }

    #[test]
    fn shorter_document_scores_higher() {
        let long = term_contribution(5, 200.0, 100.0, 1.0, 1.2, 0.75);
        let short = term_contribution(5, 50.0, 100.0, 1.0, 1.2, 0.75);
        assert!(short > long);
    }

    #[test]
    fn b_zero_ignores_length() {
        let long = term_contribution(5, 300.0, 100.0, 1.0, 1.2, 0.0);
        let short = term_contribution(5, 10.0, 100.0, 1.0, 1.2, 0.0);
        assert!((long - short).abs() < TOL);
    }

    #[test]
    fn k1_zero_reduces_to_presence() {
        // With k1=0 the saturation factor becomes tf/(tf+0) = 1 for any tf.
        assert!((term_contribution(1, 100.0, 100.0, 1.7, 0.0, 0.75) - 1.7).abs() < TOL);
        assert!((term_contribution(50, 100.0, 100.0, 1.7, 0.0, 0.75) - 1.7).abs() < TOL);
    }

    #[test]
    fn tf_saturates_below_k1_plus_1() {
        let ceiling = 1.2_f32 + 1.0;
        assert!(term_contribution(1, 100.0, 100.0, 1.0, 1.2, 0.75) < ceiling);
        assert!(term_contribution(1_000_000, 100.0, 100.0, 1.0, 1.2, 0.75) < ceiling);
    }

    #[test]
    fn degenerate_length_ratio_contributes_zero() {
        // All-zero corpus: doc_len/avgdl is 0/0 = NaN; the contribution must
        // be 0, never NaN (NaN would poison sort order downstream).
        assert_eq!(term_contribution(3, 0.0, 0.0, 1.0, 1.2, 0.75), 0.0);
    }

    // -- Corpus stats loader --------------------------------------------------

    /// Open a throwaway index database with the full schema applied.
    fn test_conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        (dir, conn)
    }

    fn insert_file(conn: &Connection, path: &str, line_count: Option<i64>) {
        conn.execute(
            "INSERT INTO files (path, language, hash, last_indexed, line_count) \
             VALUES (?1, 'rust', 'h', 0, ?2)",
            rusqlite::params![path, line_count],
        )
        .unwrap();
    }

    #[test]
    fn corpus_stats_count_and_average() {
        let (_dir, conn) = test_conn();
        insert_file(&conn, "a.rs", Some(10));
        insert_file(&conn, "b.rs", Some(30));
        let stats = load_corpus_stats(&conn).expect("stats for two files");
        assert_eq!(stats.n_docs, 2);
        assert!((stats.avg_doc_len - 20.0).abs() < TOL);
    }

    #[test]
    fn corpus_stats_excludes_null_line_counts_from_average() {
        let (_dir, conn) = test_conn();
        insert_file(&conn, "a.rs", Some(100));
        insert_file(&conn, "b.rs", None);
        let stats = load_corpus_stats(&conn).expect("stats with one measured file");
        assert_eq!(stats.n_docs, 2);
        assert!((stats.avg_doc_len - 100.0).abs() < TOL);
    }

    #[test]
    fn corpus_stats_none_when_no_files() {
        let (_dir, conn) = test_conn();
        assert!(load_corpus_stats(&conn).is_none());
    }

    #[test]
    fn corpus_stats_none_when_all_line_counts_null() {
        let (_dir, conn) = test_conn();
        insert_file(&conn, "a.rs", None);
        assert!(load_corpus_stats(&conn).is_none());
    }

    // -- rerank_lexical -------------------------------------------------------

    fn insert_term(conn: &Connection, term: &str, file: &str, tf: i64) {
        conn.execute(
            "INSERT INTO term_stats (term, file, tf) VALUES (?1, ?2, ?3)",
            rusqlite::params![term, file, tf],
        )
        .unwrap();
    }

    fn hit(file: &str, line: u64) -> SearchResult {
        SearchResult {
            file: std::path::PathBuf::from(file),
            line,
            col: 1,
            content: format!("{file}:{line}"),
        }
    }

    fn files_of(results: &[SearchResult]) -> Vec<String> {
        results
            .iter()
            .map(|r| r.file.to_string_lossy().into_owned())
            .collect()
    }

    fn default_params() -> Bm25Params {
        Bm25Params { k1: 1.2, b: 0.75 }
    }

    #[test]
    fn rerank_orders_by_tf_then_zero_scores() {
        let (_dir, conn) = test_conn();
        for (path, len) in [("a.rs", 100), ("b.rs", 100), ("c.rs", 100)] {
            insert_file(&conn, path, Some(len));
        }
        insert_term(&conn, "alpha", "a.rs", 10);
        insert_term(&conn, "alpha", "b.rs", 2);

        let results = vec![
            hit("c.rs", 1),
            hit("b.rs", 5),
            hit("a.rs", 7),
            hit("a.rs", 3),
        ];
        let ranked = rerank_lexical(&conn, &results, "alpha", default_params())
            .expect("stats present, so reranking must happen");
        // a.rs (tf 10) first with lines ascending, then b.rs (tf 2), then
        // c.rs which has no stats row at all (score 0, stays in the list).
        assert_eq!(files_of(&ranked), vec!["a.rs", "a.rs", "b.rs", "c.rs"]);
        assert_eq!(ranked[0].line, 3);
        assert_eq!(ranked[1].line, 7);
    }

    #[test]
    fn rerank_returns_permutation_of_input() {
        let (_dir, conn) = test_conn();
        for path in ["a.rs", "b.rs", "c.rs"] {
            insert_file(&conn, path, Some(80));
        }
        insert_term(&conn, "alpha", "b.rs", 9);
        insert_term(&conn, "alpha", "c.rs", 1);

        let results = vec![
            hit("a.rs", 2),
            hit("c.rs", 4),
            hit("b.rs", 6),
            hit("b.rs", 1),
        ];
        let ranked = rerank_lexical(&conn, &results, "alpha", default_params()).unwrap();
        let mut sorted_input = results.clone();
        sorted_input.sort_by(|x, y| {
            (x.file.to_string_lossy().to_string(), x.line)
                .cmp(&(y.file.to_string_lossy().to_string(), y.line))
        });
        let mut sorted_ranked = ranked.clone();
        sorted_ranked.sort_by(|x, y| {
            (x.file.to_string_lossy().to_string(), x.line)
                .cmp(&(y.file.to_string_lossy().to_string(), y.line))
        });
        assert_eq!(sorted_ranked, sorted_input, "same multiset of hits");
    }

    #[test]
    fn rerank_multi_term_sums_contributions() {
        let (_dir, conn) = test_conn();
        for path in ["a.rs", "b.rs"] {
            insert_file(&conn, path, Some(100));
        }
        // a.rs matches BOTH terms; b.rs matches only the common one.
        insert_term(&conn, "alpha", "a.rs", 5);
        insert_term(&conn, "beta", "a.rs", 5);
        insert_term(&conn, "beta", "b.rs", 5);

        let results = vec![hit("b.rs", 1), hit("a.rs", 1)];
        let ranked = rerank_lexical(&conn, &results, "alpha beta", default_params()).unwrap();
        assert_eq!(files_of(&ranked), vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn rerank_duplicate_query_terms_deduplicated() {
        let (_dir, conn) = test_conn();
        for path in ["a.rs", "b.rs"] {
            insert_file(&conn, path, Some(100));
        }
        insert_term(&conn, "alpha", "a.rs", 8);
        insert_term(&conn, "alpha", "b.rs", 1);

        let results = vec![hit("a.rs", 1), hit("b.rs", 1)];
        let once = rerank_lexical(&conn, &results, "alpha", default_params()).unwrap();
        let thrice =
            rerank_lexical(&conn, &results, "alpha alpha alpha", default_params()).unwrap();
        assert_eq!(files_of(&once), files_of(&thrice));
    }

    #[test]
    fn rerank_rare_term_beats_common_term() {
        let (_dir, conn) = test_conn();
        for path in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            insert_file(&conn, path, Some(100));
        }
        // "unicorn" appears once in the whole corpus; "value" is everywhere.
        insert_term(&conn, "unicorn", "a.rs", 1);
        for path in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            insert_term(&conn, "value", path, 10);
        }

        let results = vec![hit("b.rs", 1), hit("a.rs", 1)];
        let ranked = rerank_lexical(&conn, &results, "unicorn value", default_params()).unwrap();
        assert_eq!(files_of(&ranked), vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn rerank_ties_break_by_file_then_line() {
        let (_dir, conn) = test_conn();
        for path in ["a.rs", "b.rs"] {
            insert_file(&conn, path, Some(100));
        }
        insert_term(&conn, "alpha", "a.rs", 4);
        insert_term(&conn, "alpha", "b.rs", 4);

        let results = vec![
            hit("b.rs", 9),
            hit("b.rs", 2),
            hit("a.rs", 5),
            hit("a.rs", 1),
        ];
        let ranked = rerank_lexical(&conn, &results, "alpha", default_params()).unwrap();
        assert_eq!(files_of(&ranked), vec!["a.rs", "a.rs", "b.rs", "b.rs"]);
        assert_eq!(ranked[0].line, 1);
        assert_eq!(ranked[1].line, 5);
        assert_eq!(ranked[2].line, 2);
        assert_eq!(ranked[3].line, 9);
    }

    #[test]
    fn rerank_unknown_file_uses_average_length() {
        let (_dir, conn) = test_conn();
        insert_file(&conn, "a.rs", Some(50));
        insert_file(&conn, "b.rs", Some(150));
        // c.rs is a candidate but has no files row: neutral length (avgdl).
        insert_term(&conn, "alpha", "a.rs", 2);
        insert_term(&conn, "alpha", "b.rs", 2);
        insert_term(&conn, "alpha", "c.rs", 2);

        let ranked = rerank_lexical(&conn, &[hit("c.rs", 1)], "alpha", default_params());
        // Must not panic; c.rs survives regardless of exact placement.
        assert!(ranked.is_some());
        assert_eq!(files_of(&ranked.unwrap()), vec!["c.rs"]);
    }

    #[test]
    fn rerank_shorter_document_outranks_longer() {
        let (_dir, conn) = test_conn();
        insert_file(&conn, "long.rs", Some(400));
        insert_file(&conn, "short.rs", Some(20));
        insert_term(&conn, "alpha", "long.rs", 3);
        insert_term(&conn, "alpha", "short.rs", 3);

        let results = vec![hit("long.rs", 1), hit("short.rs", 1)];
        let ranked = rerank_lexical(&conn, &results, "alpha", default_params()).unwrap();
        assert_eq!(files_of(&ranked), vec!["short.rs", "long.rs"]);
    }

    #[test]
    fn rerank_punctuation_only_query_returns_input_unchanged() {
        let (_dir, conn) = test_conn();
        insert_file(&conn, "a.rs", Some(10));
        insert_term(&conn, "alpha", "a.rs", 3);

        let results = vec![hit("a.rs", 1), hit("a.rs", 2)];
        let ranked = rerank_lexical(&conn, &results, ":: - _", default_params()).unwrap();
        assert_eq!(ranked, results);
    }

    #[test]
    fn rerank_empty_results_returned_unchanged() {
        let (_dir, conn) = test_conn();
        insert_file(&conn, "a.rs", Some(10));
        insert_term(&conn, "alpha", "a.rs", 3);
        let ranked = rerank_lexical(&conn, &[], "alpha", default_params()).unwrap();
        assert!(ranked.is_empty());
    }

    // -- rerank fallback (pre-V5 / broken stats) ------------------------------

    #[test]
    fn rerank_none_when_term_stats_empty() {
        let (_dir, conn) = test_conn();
        insert_file(&conn, "a.rs", Some(10));
        let results = vec![hit("a.rs", 1)];
        assert!(rerank_lexical(&conn, &results, "alpha", default_params()).is_none());
    }

    #[test]
    fn rerank_none_when_term_stats_table_dropped() {
        let (_dir, conn) = test_conn();
        insert_file(&conn, "a.rs", Some(10));
        insert_term(&conn, "alpha", "a.rs", 3);
        conn.execute("DROP TABLE term_stats", []).unwrap();
        let results = vec![hit("a.rs", 1)];
        assert!(rerank_lexical(&conn, &results, "alpha", default_params()).is_none());
    }

    #[test]
    fn rerank_none_when_no_files_rows() {
        let (_dir, conn) = test_conn();
        // Stats exist but the files table is empty: degenerate corpus.
        insert_term(&conn, "alpha", "a.rs", 3);
        let results = vec![hit("a.rs", 1)];
        assert!(rerank_lexical(&conn, &results, "alpha", default_params()).is_none());
    }

    // -- Benchmark (manual gate, run in release) ------------------------------

    /// Measure the warm-query cost of `rerank_lexical` on the TASK-078-style
    /// synthetic corpus (300 files x 150 lines, 500-word Zipf vocabulary,
    /// seeded so every run measures the identical corpus). For each of five
    /// literal patterns it reports the median of 30 timed runs for
    /// `text_search` alone, `text_search` + rerank combined, and rerank only
    /// (the delta). Manual acceptance gate (PRD-BM25-REQ-006): every
    /// rerank-only median must stay below 10ms. No timing assertion — this is
    /// a measurement harness:
    /// `cargo test --release bench_bm25_warm_query_overhead -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_bm25_warm_query_overhead() {
        use rand::SeedableRng;
        use std::time::Instant;

        fn zipf_pick(rng: &mut rand::rngs::StdRng, vocab_len: usize) -> usize {
            use rand::Rng;
            let u: f64 = rng.r#gen();
            ((vocab_len as f64) * u * u).floor() as usize % vocab_len
        }

        fn write_bench_corpus(root: &std::path::Path) {
            use std::fs;
            fs::create_dir(root.join(".git")).unwrap();
            fs::create_dir(root.join("src")).unwrap();

            let mut rng = rand::rngs::StdRng::seed_from_u64(79);
            let vocab: Vec<String> = (0..500).map(|i| format!("w{i}")).collect();
            for file_idx in 0..300 {
                let mut lines = vec![format!("fn w{file_idx}_entry() {{")];
                while lines.len() < 150 {
                    let picks: Vec<&str> = (0..6)
                        .map(|_| vocab[zipf_pick(&mut rng, vocab.len())].as_str())
                        .collect();
                    lines.push(format!("    let value = {} + {};", picks[0], picks[1]));
                    lines.push(format!(
                        "    call_{}({}, {});",
                        picks[2], picks[3], picks[4]
                    ));
                    if lines.len() >= 150 {
                        break;
                    }
                    lines.push(format!("    // {} {} {}", picks[5], picks[0], picks[2]));
                }
                lines.push("}".to_string());
                fs::write(
                    root.join("src").join(format!("mod_{file_idx:03}.rs")),
                    lines.join("\n"),
                )
                .unwrap();
            }
        }

        fn median(durations: &mut Vec<std::time::Duration>) -> std::time::Duration {
            durations.sort();
            durations[durations.len() / 2]
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_bench_corpus(root);
        crate::pipeline::build_index(root, true).unwrap();
        let index_path = crate::db::find_existing_index(root).unwrap();
        let conn = crate::db::open(&index_path).unwrap();

        let root_str = root.display().to_string();
        let patterns = [
            ("frequent", "w0"),
            ("frequent-2", "w1"),
            ("rare", "w120"),
            ("rarest", "w499"),
            ("multi-term", "let value"),
        ];
        let params = default_params();
        let runs = 30;

        for (label, pattern) in patterns {
            // Frozen candidate list: walker-relative so paths match
            // term_stats.file keys, as in the router (cwd = repo root).
            let mut candidates = crate::search::text_search(pattern, false, false, &[root_str.clone()])
                .unwrap();
            for result in &mut candidates {
                if let Ok(rel) = result.file.strip_prefix(root) {
                    result.file = rel.to_path_buf();
                }
            }

            // Warm-up: first call pays statement preparation and page cache.
            rerank_lexical(&conn, &candidates, pattern, params).unwrap();

            let mut search_times = Vec::with_capacity(runs);
            let mut combined_times = Vec::with_capacity(runs);
            let mut rerank_times = Vec::with_capacity(runs);
            for _ in 0..runs {
                let start = Instant::now();
                let mut found =
                    crate::search::text_search(pattern, false, false, &[root_str.clone()]).unwrap();
                let search_elapsed = start.elapsed();
                for result in &mut found {
                    if let Ok(rel) = result.file.strip_prefix(root) {
                        result.file = rel.to_path_buf();
                    }
                }

                let start = Instant::now();
                rerank_lexical(&conn, &found, pattern, params).unwrap();
                let rerank_elapsed = start.elapsed();

                search_times.push(search_elapsed);
                rerank_times.push(rerank_elapsed);
                combined_times.push(search_elapsed + rerank_elapsed);
            }

            let search_median = median(&mut search_times);
            let rerank_median = median(&mut rerank_times);
            let combined_median = median(&mut combined_times);
            let gate: std::time::Duration = std::time::Duration::from_millis(10);
            println!(
                "bench {label:<11} candidates={:<5} search={search_median:>9?} \
                 combined={combined_median:>9?} rerank={rerank_median:>9?} \
                 gate(<10ms): {}",
                candidates.len(),
                if rerank_median < gate { "PASS" } else { "FAIL" },
            );
        }
    }
}
