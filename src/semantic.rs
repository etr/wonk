//! Brute-force cosine similarity search over stored embedding vectors.
//!
//! Provides parallel vector similarity computation using rayon and
//! result resolution against the symbols table.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use rayon::prelude::*;
use rusqlite::Connection;

use crate::errors::{DbError, EmbeddingError};
use crate::types::{SemanticResult, SymbolKind};

/// Compute the dot product of two f32 slices.
///
/// For L2-normalized vectors, the dot product equals the cosine similarity.
/// In debug builds, panics if the slices have different lengths.  In release
/// builds, mismatched lengths silently compute over the shorter length.
pub fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "dot_product: dimension mismatch");
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Find the top-N most similar embeddings to a query vector.
///
/// Computes the dot product (cosine similarity for normalized vectors)
/// between `query_vec` and every vector in `all_embeddings` using rayon
/// for parallel computation.  Returns `(symbol_id, score)` pairs sorted
/// by descending similarity, truncated to `limit`.
pub fn semantic_search(
    query_vec: &[f32],
    all_embeddings: &[(i64, Vec<f32>)],
    limit: usize,
) -> Vec<(i64, f32)> {
    if all_embeddings.is_empty() || limit == 0 {
        return Vec::new();
    }

    let mut scored: Vec<(i64, f32)> = all_embeddings
        .par_iter()
        .map(|(id, vec)| (*id, dot_product(query_vec, vec)))
        .collect();

    scored.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

/// Resolve scored symbol IDs into full [`SemanticResult`] structs.
///
/// Joins each `(symbol_id, score)` pair with the `symbols` table to fetch
/// file, line, name, and kind.  Preserves the input ordering (by descending
/// score).  Symbols not found in the database are silently skipped.
pub fn resolve_results(
    conn: &Connection,
    scored: &[(i64, f32)],
) -> Result<Vec<SemanticResult>, EmbeddingError> {
    if scored.is_empty() {
        return Ok(Vec::new());
    }

    // Build an IN (...) clause for all symbol IDs.
    let placeholders: Vec<String> = (1..=scored.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "SELECT id, name, kind, file, line FROM symbols WHERE id IN ({})",
        placeholders.join(", ")
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

    let params: Vec<&dyn rusqlite::types::ToSql> = scored
        .iter()
        .map(|(id, _)| id as &dyn rusqlite::types::ToSql)
        .collect();

    let rows = stmt
        .query_map(params.as_slice(), |row| {
            let id: i64 = row.get(0)?;
            let name: String = row.get(1)?;
            let kind_str: String = row.get(2)?;
            let file: String = row.get(3)?;
            let line: usize = row.get::<_, i64>(4)? as usize;
            Ok((id, name, kind_str, file, line))
        })
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

    // Small struct to avoid opaque positional tuples in the lookup map.
    struct SymbolFields {
        name: String,
        kind_str: String,
        file: String,
        line: usize,
    }

    // Collect into a map keyed by symbol_id for order-preserving lookup.
    let mut by_id: HashMap<i64, SymbolFields> = HashMap::with_capacity(scored.len());
    for r in rows {
        let (id, name, kind_str, file, line) =
            r.map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;
        by_id.insert(
            id,
            SymbolFields {
                name,
                kind_str,
                file,
                line,
            },
        );
    }

    // Reconstruct results in the original scored order, skipping missing IDs.
    let results = scored
        .iter()
        .filter_map(|(id, score)| {
            by_id.get(id).map(|sym| {
                let kind = sym
                    .kind_str
                    .parse::<SymbolKind>()
                    .unwrap_or(SymbolKind::Function);
                SemanticResult {
                    symbol_id: *id,
                    file: sym.file.clone(),
                    line: sym.line,
                    symbol_name: sym.name.clone(),
                    symbol_kind: kind,
                    similarity_score: *score,
                }
            })
        })
        .collect();

    Ok(results)
}

/// Filter out semantic results that overlap with structural search results.
///
/// Any semantic result whose `(file, line)` pair appears in `structural_keys`
/// is excluded, preventing the same location from appearing twice in blended
/// output.
pub fn dedup_semantic<'a>(
    semantic: &'a [SemanticResult],
    structural_keys: &HashSet<(String, u64)>,
) -> Vec<&'a SemanticResult> {
    semantic
        .iter()
        .filter(|sr| !structural_keys.contains(&(sr.file.clone(), sr.line as u64)))
        .collect()
}

// ---------------------------------------------------------------------------
// Dependency graph traversal
// ---------------------------------------------------------------------------

/// Extract the stem from an import path for fuzzy matching against file paths.
///
/// Takes the last segment of an import path (splitting on `/`, `::`, or `:`)
/// and strips any file extension. For example:
/// - `./utils` -> `utils`
/// - `crate::db` -> `db`
/// - `../helpers/format.ts` -> `format`
fn extract_import_stem(import_path: &str) -> Option<String> {
    // Split on all known separators and take the last non-empty segment.
    let last_segment = import_path
        .rsplit(['/', ':'])
        .find(|s| !s.is_empty())?;

    // Strip file extension if present.
    let stem = Path::new(last_segment)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| last_segment.to_string());

    if stem.is_empty() {
        None
    } else {
        Some(stem)
    }
}

/// Adjacency list mapping file paths to their connected neighbors.
pub(crate) type AdjacencyList = HashMap<String, HashSet<String>>;

/// Which direction(s) of the dependency graph to build.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DepDirection {
    Forward,
    Reverse,
    Both,
}

/// Load the file-level dependency graph from SQLite into adjacency lists.
///
/// Returns `(forward, reverse)` where:
/// - `forward[file]` = set of files that `file` imports (empty if not requested)
/// - `reverse[file]` = set of files that import `file` (empty if not requested)
///
/// Use [`DepDirection`] to build only the direction you need, avoiding
/// unnecessary allocations.
pub(crate) fn load_dep_graph(
    conn: &Connection,
    direction: DepDirection,
) -> Result<(AdjacencyList, AdjacencyList), DbError> {
    // Build a stem -> set of file paths lookup from the files table.
    let mut stem_to_files: HashMap<String, Vec<String>> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT path FROM files")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            let path = row?;
            if let Some(stem) = Path::new(&path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
            {
                stem_to_files
                    .entry(stem)
                    .or_default()
                    .push(path.clone());
            }
        }
    }

    let build_forward = direction == DepDirection::Forward || direction == DepDirection::Both;
    let build_reverse = direction == DepDirection::Reverse || direction == DepDirection::Both;

    let mut forward: AdjacencyList = HashMap::new();
    let mut reverse: AdjacencyList = HashMap::new();

    // Query all import edges.
    {
        let mut stmt = conn.prepare("SELECT source_file, import_path FROM file_imports")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (source_file, import_path) = row?;
            if let Some(stem) = extract_import_stem(&import_path)
                && let Some(targets) = stem_to_files.get(&stem)
            {
                for target in targets {
                    if target != &source_file {
                        if build_forward {
                            forward
                                .entry(source_file.clone())
                                .or_default()
                                .insert(target.clone());
                        }
                        if build_reverse {
                            reverse
                                .entry(target.clone())
                                .or_default()
                                .insert(source_file.clone());
                        }
                    }
                }
            }
        }
    }

    Ok((forward, reverse))
}

/// Standard BFS traversal over an adjacency list.
///
/// Returns the set of all reachable nodes from `start`, including `start`
/// itself. Handles cycles via a visited set.
fn bfs(start: &str, adj: &AdjacencyList) -> HashSet<String> {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();

    let start_owned = start.to_string();
    visited.insert(start_owned.clone());
    queue.push_back(start_owned);

    while let Some(node) = queue.pop_front() {
        if let Some(neighbors) = adj.get(&node) {
            for neighbor in neighbors {
                if visited.insert(neighbor.clone()) {
                    queue.push_back(neighbor.clone());
                }
            }
        }
    }

    visited
}

/// Compute the set of files transitively reachable from `file` by following
/// import edges forward.
///
/// For example, if A imports B and B imports C, then `reachable_from(conn, "A")`
/// returns `{A, B, C}`. The starting file is always included in the result.
pub fn reachable_from(conn: &Connection, file: &str) -> Result<HashSet<String>, DbError> {
    let (forward, _) = load_dep_graph(conn, DepDirection::Forward)?;
    Ok(bfs(file, &forward))
}

/// Compute the set of files that transitively import `file` (reverse
/// reachability).
///
/// For example, if A imports B and C imports B, then `reachable_to(conn, "B")`
/// returns `{A, B, C}`. The target file is always included in the result.
pub fn reachable_to(conn: &Connection, file: &str) -> Result<HashSet<String>, DbError> {
    let (_, reverse) = load_dep_graph(conn, DepDirection::Reverse)?;
    Ok(bfs(file, &reverse))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // dot_product tests
    // -----------------------------------------------------------------------

    #[test]
    fn dot_product_basic() {
        let a = [1.0_f32, 2.0, 3.0];
        let b = [4.0_f32, 5.0, 6.0];
        // 1*4 + 2*5 + 3*6 = 4 + 10 + 18 = 32
        assert!((dot_product(&a, &b) - 32.0).abs() < 1e-6);
    }

    #[test]
    fn dot_product_identical_normalized() {
        // Normalized vector dotted with itself should be ~1.0
        let v = [0.6_f32, 0.8];
        let result = dot_product(&v, &v);
        assert!((result - 1.0).abs() < 1e-6);
    }

    #[test]
    fn dot_product_opposite() {
        let a = [1.0_f32, 0.0];
        let b = [-1.0_f32, 0.0];
        assert!((dot_product(&a, &b) - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn dot_product_orthogonal() {
        let a = [1.0_f32, 0.0];
        let b = [0.0_f32, 1.0];
        assert!((dot_product(&a, &b)).abs() < 1e-6);
    }

    #[test]
    fn dot_product_empty() {
        let a: [f32; 0] = [];
        let b: [f32; 0] = [];
        assert!((dot_product(&a, &b)).abs() < 1e-6);
    }

    // -----------------------------------------------------------------------
    // semantic_search tests
    // -----------------------------------------------------------------------

    #[test]
    fn semantic_search_empty_embeddings() {
        let query = [1.0_f32, 0.0];
        let result = semantic_search(&query, &[], 10);
        assert!(result.is_empty());
    }

    #[test]
    fn semantic_search_zero_limit() {
        let query = [1.0_f32, 0.0];
        let embeddings = vec![(1, vec![1.0_f32, 0.0])];
        let result = semantic_search(&query, &embeddings, 0);
        assert!(result.is_empty());
    }

    #[test]
    fn semantic_search_sorted_descending() {
        let query = [1.0_f32, 0.0];
        let embeddings = vec![
            (1, vec![0.0_f32, 1.0]), // score = 0.0
            (2, vec![0.6_f32, 0.8]), // score = 0.6
            (3, vec![1.0_f32, 0.0]), // score = 1.0
            (4, vec![0.8_f32, 0.6]), // score = 0.8
        ];
        let result = semantic_search(&query, &embeddings, 10);
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].0, 3); // highest score (1.0)
        assert_eq!(result[1].0, 4); // 0.8
        assert_eq!(result[2].0, 2); // 0.6
        assert_eq!(result[3].0, 1); // 0.0
    }

    #[test]
    fn semantic_search_respects_limit() {
        let query = [1.0_f32, 0.0];
        let embeddings = vec![
            (1, vec![0.0_f32, 1.0]),
            (2, vec![0.6_f32, 0.8]),
            (3, vec![1.0_f32, 0.0]),
            (4, vec![0.8_f32, 0.6]),
        ];
        let result = semantic_search(&query, &embeddings, 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, 3); // highest
        assert_eq!(result[1].0, 4); // second highest
    }

    #[test]
    fn semantic_search_correct_scores() {
        let query = [0.6_f32, 0.8];
        let embeddings = vec![
            (1, vec![1.0_f32, 0.0]), // score = 0.6
            (2, vec![0.0_f32, 1.0]), // score = 0.8
        ];
        let result = semantic_search(&query, &embeddings, 10);
        assert_eq!(result.len(), 2);
        assert!((result[0].1 - 0.8).abs() < 1e-6); // id=2
        assert!((result[1].1 - 0.6).abs() < 1e-6); // id=1
    }

    #[test]
    fn semantic_search_large_input_performance() {
        // 50K vectors of dimension 768 -- should complete in well under 200ms.
        let dim = 768;
        let n = 50_000;
        let query: Vec<f32> = {
            let mut v: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.001).collect();
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in v.iter_mut() {
                    *x /= norm;
                }
            }
            v
        };

        let embeddings: Vec<(i64, Vec<f32>)> = (0..n as i64)
            .map(|i| {
                let mut v: Vec<f32> = (0..dim).map(|j| ((i + j as i64) as f32) * 0.0001).collect();
                let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for x in v.iter_mut() {
                        *x /= norm;
                    }
                }
                (i, v)
            })
            .collect();

        let result = semantic_search(&query, &embeddings, 10);

        assert_eq!(result.len(), 10);
        // Results should be sorted descending
        for w in result.windows(2) {
            assert!(w[0].1 >= w[1].1);
        }
    }

    /// The 200ms budget applies to optimized (release) builds only.
    /// Debug builds are significantly slower due to lack of auto-vectorization.
    #[test]
    #[cfg(not(debug_assertions))]
    fn semantic_search_large_input_perf_budget() {
        let dim = 768;
        let n = 50_000;
        let query: Vec<f32> = {
            let mut v: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.001).collect();
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in v.iter_mut() {
                    *x /= norm;
                }
            }
            v
        };
        let embeddings: Vec<(i64, Vec<f32>)> = (0..n as i64)
            .map(|i| {
                let mut v: Vec<f32> = (0..dim).map(|j| ((i + j as i64) as f32) * 0.0001).collect();
                let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for x in v.iter_mut() {
                        *x /= norm;
                    }
                }
                (i, v)
            })
            .collect();

        let start = std::time::Instant::now();
        let _result = semantic_search(&query, &embeddings, 10);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 200,
            "50K search took {}ms, expected < 200ms",
            elapsed.as_millis()
        );
    }

    // -----------------------------------------------------------------------
    // resolve_results tests
    // -----------------------------------------------------------------------

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE symbols (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                file TEXT NOT NULL,
                line INTEGER NOT NULL,
                col INTEGER NOT NULL,
                end_line INTEGER,
                scope TEXT,
                signature TEXT,
                language TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn insert_symbol(conn: &Connection, name: &str, kind: &str, file: &str, line: i64) -> i64 {
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, signature, language)
             VALUES (?1, ?2, ?3, ?4, 0, '', 'Rust')",
            rusqlite::params![name, kind, file, line],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn resolve_results_empty_input() {
        let conn = setup_test_db();
        let result = resolve_results(&conn, &[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_results_basic_join() {
        let conn = setup_test_db();
        let id = insert_symbol(&conn, "foo", "function", "src/lib.rs", 42);

        let scored = vec![(id, 0.95_f32)];
        let results = resolve_results(&conn, &scored).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol_id, id);
        assert_eq!(results[0].file, "src/lib.rs");
        assert_eq!(results[0].line, 42);
        assert_eq!(results[0].symbol_name, "foo");
        assert_eq!(results[0].symbol_kind, SymbolKind::Function);
        assert!((results[0].similarity_score - 0.95).abs() < 1e-6);
    }

    #[test]
    fn resolve_results_preserves_score_order() {
        let conn = setup_test_db();
        let id1 = insert_symbol(&conn, "alpha", "function", "a.rs", 1);
        let id2 = insert_symbol(&conn, "beta", "method", "b.rs", 10);
        let id3 = insert_symbol(&conn, "gamma", "class", "c.rs", 20);

        // Scored in descending order: id2, id3, id1
        let scored = vec![(id2, 0.9_f32), (id3, 0.7_f32), (id1, 0.5_f32)];
        let results = resolve_results(&conn, &scored).unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].symbol_name, "beta");
        assert_eq!(results[0].symbol_kind, SymbolKind::Method);
        assert_eq!(results[1].symbol_name, "gamma");
        assert_eq!(results[1].symbol_kind, SymbolKind::Class);
        assert_eq!(results[2].symbol_name, "alpha");
        assert_eq!(results[2].symbol_kind, SymbolKind::Function);
    }

    #[test]
    fn resolve_results_skips_missing_symbols() {
        let conn = setup_test_db();
        let id = insert_symbol(&conn, "exists", "function", "a.rs", 1);

        // Include a non-existent symbol_id (999)
        let scored = vec![(id, 0.9_f32), (999, 0.5_f32)];
        let results = resolve_results(&conn, &scored).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol_name, "exists");
    }

    // -----------------------------------------------------------------------
    // dedup_semantic tests
    // -----------------------------------------------------------------------

    fn make_semantic_result(file: &str, line: usize, score: f32) -> SemanticResult {
        SemanticResult {
            symbol_id: 0,
            file: file.to_string(),
            line,
            symbol_name: "test".to_string(),
            symbol_kind: SymbolKind::Function,
            similarity_score: score,
        }
    }

    #[test]
    fn dedup_semantic_removes_overlapping_results() {
        use std::collections::HashSet;
        let semantic = vec![
            make_semantic_result("src/lib.rs", 10, 0.95),
            make_semantic_result("src/lib.rs", 20, 0.85),
        ];
        let mut structural_keys = HashSet::new();
        structural_keys.insert(("src/lib.rs".to_string(), 10u64));

        let deduped = dedup_semantic(&semantic, &structural_keys);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].line, 20);
    }

    #[test]
    fn dedup_semantic_preserves_non_overlapping() {
        use std::collections::HashSet;
        let semantic = vec![
            make_semantic_result("src/a.rs", 5, 0.9),
            make_semantic_result("src/b.rs", 10, 0.8),
        ];
        let structural_keys = HashSet::new();

        let deduped = dedup_semantic(&semantic, &structural_keys);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn dedup_semantic_empty_input() {
        use std::collections::HashSet;
        let semantic: Vec<SemanticResult> = vec![];
        let structural_keys = HashSet::new();

        let deduped = dedup_semantic(&semantic, &structural_keys);
        assert!(deduped.is_empty());
    }

    #[test]
    fn dedup_semantic_all_overlap() {
        use std::collections::HashSet;
        let semantic = vec![
            make_semantic_result("src/a.rs", 5, 0.9),
            make_semantic_result("src/b.rs", 10, 0.8),
        ];
        let mut structural_keys = HashSet::new();
        structural_keys.insert(("src/a.rs".to_string(), 5u64));
        structural_keys.insert(("src/b.rs".to_string(), 10u64));

        let deduped = dedup_semantic(&semantic, &structural_keys);
        assert!(deduped.is_empty());
    }

    // -----------------------------------------------------------------------
    // Dependency graph traversal tests
    // -----------------------------------------------------------------------

    fn setup_dep_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY,
                language TEXT,
                hash TEXT NOT NULL,
                last_indexed INTEGER NOT NULL,
                line_count INTEGER,
                symbols_count INTEGER
            );
            CREATE TABLE IF NOT EXISTS file_imports (
                id INTEGER PRIMARY KEY,
                source_file TEXT NOT NULL,
                import_path TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn insert_file(conn: &Connection, path: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO files (path, language, hash, last_indexed) \
             VALUES (?1, 'TypeScript', 'abc123', 0)",
            rusqlite::params![path],
        )
        .unwrap();
    }

    fn insert_import(conn: &Connection, source: &str, import: &str) {
        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params![source, import],
        )
        .unwrap();
    }

    #[test]
    fn test_reachable_from_linear_chain() {
        // A imports B, B imports C => reachable_from(A) = {A, B, C}
        let conn = setup_dep_test_db();
        insert_file(&conn, "src/a.ts");
        insert_file(&conn, "src/b.ts");
        insert_file(&conn, "src/c.ts");
        insert_import(&conn, "src/a.ts", "./b");
        insert_import(&conn, "src/b.ts", "./c");

        let result = reachable_from(&conn, "src/a.ts").unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.contains("src/a.ts"));
        assert!(result.contains("src/b.ts"));
        assert!(result.contains("src/c.ts"));
    }

    #[test]
    fn test_reachable_to_diamond() {
        // A imports B, C imports B => reachable_to(B) = {A, B, C}
        let conn = setup_dep_test_db();
        insert_file(&conn, "src/a.ts");
        insert_file(&conn, "src/b.ts");
        insert_file(&conn, "src/c.ts");
        insert_import(&conn, "src/a.ts", "./b");
        insert_import(&conn, "src/c.ts", "./b");

        let result = reachable_to(&conn, "src/b.ts").unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.contains("src/a.ts"));
        assert!(result.contains("src/b.ts"));
        assert!(result.contains("src/c.ts"));
    }

    #[test]
    fn test_reachable_from_includes_self() {
        // No imports, reachable_from(A) = {A}
        let conn = setup_dep_test_db();
        insert_file(&conn, "src/a.ts");

        let result = reachable_from(&conn, "src/a.ts").unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains("src/a.ts"));
    }

    #[test]
    fn test_reachable_to_includes_self() {
        // No reverse deps, reachable_to(A) = {A}
        let conn = setup_dep_test_db();
        insert_file(&conn, "src/a.ts");

        let result = reachable_to(&conn, "src/a.ts").unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains("src/a.ts"));
    }

    #[test]
    fn test_reachable_from_cycle() {
        // A imports B, B imports A => reachable_from(A) = {A, B}
        let conn = setup_dep_test_db();
        insert_file(&conn, "src/a.ts");
        insert_file(&conn, "src/b.ts");
        insert_import(&conn, "src/a.ts", "./b");
        insert_import(&conn, "src/b.ts", "./a");

        let result = reachable_from(&conn, "src/a.ts").unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains("src/a.ts"));
        assert!(result.contains("src/b.ts"));
    }

    #[test]
    fn test_reachable_from_complex_graph() {
        // Diamond: A->B->D, A->C->D => reachable_from(A) = {A,B,C,D}
        let conn = setup_dep_test_db();
        insert_file(&conn, "src/a.ts");
        insert_file(&conn, "src/b.ts");
        insert_file(&conn, "src/c.ts");
        insert_file(&conn, "src/d.ts");
        insert_import(&conn, "src/a.ts", "./b");
        insert_import(&conn, "src/a.ts", "./c");
        insert_import(&conn, "src/b.ts", "./d");
        insert_import(&conn, "src/c.ts", "./d");

        let result = reachable_from(&conn, "src/a.ts").unwrap();
        assert_eq!(result.len(), 4);
        assert!(result.contains("src/a.ts"));
        assert!(result.contains("src/b.ts"));
        assert!(result.contains("src/c.ts"));
        assert!(result.contains("src/d.ts"));
    }

    #[test]
    fn test_reachable_to_transitive_chain() {
        // A imports B, B imports C => reachable_to(C) = {A, B, C}
        let conn = setup_dep_test_db();
        insert_file(&conn, "src/a.ts");
        insert_file(&conn, "src/b.ts");
        insert_file(&conn, "src/c.ts");
        insert_import(&conn, "src/a.ts", "./b");
        insert_import(&conn, "src/b.ts", "./c");

        let result = reachable_to(&conn, "src/c.ts").unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.contains("src/a.ts"));
        assert!(result.contains("src/b.ts"));
        assert!(result.contains("src/c.ts"));
    }

    #[test]
    fn test_reachable_from_unknown_file() {
        // Start file not in DB => returns {start_file}
        let conn = setup_dep_test_db();

        let result = reachable_from(&conn, "src/unknown.ts").unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains("src/unknown.ts"));
    }

    #[test]
    fn test_reachable_from_stem_matching() {
        // Realistic import_path like ./utils matching src/utils.ts
        let conn = setup_dep_test_db();
        insert_file(&conn, "src/main.ts");
        insert_file(&conn, "src/utils.ts");
        insert_file(&conn, "src/helpers.ts");
        insert_import(&conn, "src/main.ts", "./utils");
        insert_import(&conn, "src/utils.ts", "../helpers");

        let result = reachable_from(&conn, "src/main.ts").unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.contains("src/main.ts"));
        assert!(result.contains("src/utils.ts"));
        assert!(result.contains("src/helpers.ts"));
    }
}
