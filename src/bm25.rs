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

use rusqlite::Connection;

use crate::config::SearchConfig;

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
/// A non-positive denominator (pathological params, e.g. negative k1)
/// contributes 0 rather than NaN.
#[allow(clippy::too_many_arguments)]
pub fn term_contribution(tf: u64, doc_len: f32, avgdl: f32, idf: f32, k1: f32, b: f32) -> f32 {
    let tf = tf as f32;
    let denominator = tf + k1 * (1.0 - b + b * doc_len / avgdl);
    if denominator <= 0.0 {
        return 0.0;
    }
    idf * tf * (k1 + 1.0) / denominator
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
}
