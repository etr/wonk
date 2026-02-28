//! Structural summary engine for `wonk summary`.
//!
//! Queries the SQLite index to aggregate structural metrics (file count, line
//! count, symbol counts by kind, language breakdown, dependency count) for a
//! given path. Supports three detail levels and recursive depth traversal.

use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

use crate::types::{DetailLevel, SummaryMetrics, SummaryPathType, SummaryResult};

/// Maximum recursion depth to prevent unbounded resource consumption.
const MAX_RECURSIVE_DEPTH: usize = 20;

/// Options for the summary engine.
pub struct SummaryOptions {
    /// Detail level for the output.
    pub detail: DetailLevel,
    /// Maximum recursion depth. `None` means unlimited (up to `MAX_RECURSIVE_DEPTH`).
    pub depth: Option<usize>,
    /// Whether to suppress stderr hints.
    pub suppress: bool,
}

/// Escape SQLite LIKE metacharacters (`%` and `_`) in a string.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Summarize a path (file or directory) using the index.
///
/// The path is relative to the repo root. Directories use `prefix/` LIKE
/// patterns to avoid false matches (e.g. `src` won't match `src_utils.rs`).
/// Files use exact match.
///
/// Returns a `SummaryResult` with zero metrics for empty/unknown paths
/// (not an error), consistent with other wonk commands.
///
/// The `_repo_root` parameter is reserved for the `--semantic` path (TASK-064).
pub fn summarize_path(
    conn: &Connection,
    path: &str,
    _repo_root: &Path,
    options: &SummaryOptions,
) -> Result<SummaryResult> {
    let normalized = normalize_path(path);
    let path_type = detect_path_type(conn, &normalized)?;

    // Build LIKE pattern and exact path for queries.
    // Files use exact match only; directories use `prefix/%` LIKE pattern.
    let (like_pattern, exact_path) = match path_type {
        SummaryPathType::File => (normalized.clone(), normalized.clone()),
        SummaryPathType::Directory => {
            let prefix = if normalized.ends_with('/') {
                normalized.clone()
            } else {
                format!("{normalized}/")
            };
            let safe_prefix = escape_like(&prefix);
            (format!("{safe_prefix}%"), prefix)
        }
    };

    let metrics = query_metrics(conn, &like_pattern, &exact_path, path_type)?;

    // Recurse into children if depth allows.
    let children = if should_recurse(options.depth, 0) && path_type == SummaryPathType::Directory {
        // Pre-fetch all subtree data in exactly 4 SQL queries, then build
        // the entire child tree in-memory with zero additional SQL.
        let subtree_data = SubtreeData::load(conn, &like_pattern)?;
        build_children_from_data(&subtree_data, &exact_path, options, 1)?
    } else {
        vec![]
    };

    Ok(SummaryResult {
        path: normalized,
        path_type,
        detail_level: options.detail,
        metrics,
        children,
        description: None,
    })
}

/// Normalize a user-supplied path for DB queries.
fn normalize_path(path: &str) -> String {
    let p = path.trim_start_matches("./");
    let p = p.trim_end_matches('/');
    if p.is_empty() {
        ".".to_string()
    } else {
        p.to_string()
    }
}

/// Check whether a path refers to a file or directory in the index.
fn detect_path_type(conn: &Connection, path: &str) -> Result<SummaryPathType> {
    let file_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files WHERE path = ?1",
        rusqlite::params![path],
        |row| row.get(0),
    )?;

    if file_count > 0 {
        return Ok(SummaryPathType::File);
    }

    Ok(SummaryPathType::Directory)
}

/// Query aggregated metrics for a path using the LIKE pattern and exact path.
fn query_metrics(
    conn: &Connection,
    like_pattern: &str,
    exact_path: &str,
    path_type: SummaryPathType,
) -> Result<SummaryMetrics> {
    // For files, use exact match only (more efficient, avoids LIKE overhead).
    // For directories, use LIKE pattern with ESCAPE clause.
    let (file_query, sym_query, dep_query) = match path_type {
        SummaryPathType::File => (
            "SELECT language, COUNT(*), COALESCE(SUM(line_count), 0) \
             FROM files WHERE path = ?1 \
             GROUP BY language ORDER BY language",
            "SELECT kind, COUNT(*) FROM symbols \
             WHERE file = ?1 \
             GROUP BY kind ORDER BY kind",
            "SELECT COUNT(DISTINCT import_path) FROM file_imports \
             WHERE source_file = ?1",
        ),
        SummaryPathType::Directory => (
            "SELECT language, COUNT(*), COALESCE(SUM(line_count), 0) \
             FROM files WHERE path LIKE ?1 ESCAPE '\\' \
             GROUP BY language ORDER BY language",
            "SELECT kind, COUNT(*) FROM symbols \
             WHERE file LIKE ?1 ESCAPE '\\' \
             GROUP BY kind ORDER BY kind",
            "SELECT COUNT(DISTINCT import_path) FROM file_imports \
             WHERE source_file LIKE ?1 ESCAPE '\\'",
        ),
    };

    // The query param is exact_path for files, like_pattern for directories.
    let param = match path_type {
        SummaryPathType::File => exact_path,
        SummaryPathType::Directory => like_pattern,
    };

    // Use prepare_cached for LRU statement caching — avoids re-parsing
    // identical SQL strings on repeated calls during recursive traversal.
    let mut lang_stmt = conn.prepare_cached(file_query)?;
    let lang_rows: Vec<(String, usize, usize)> = lang_stmt
        .query_map(rusqlite::params![param], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, i64>(2)? as usize,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut file_count = 0usize;
    let mut line_count = 0usize;
    let mut language_breakdown: Vec<(String, usize)> = Vec::new();

    for (lang, count, lines) in &lang_rows {
        file_count += count;
        line_count += lines;
        language_breakdown.push((lang.clone(), *count));
    }

    // Symbol counts by kind.
    let mut sym_stmt = conn.prepare_cached(sym_query)?;
    let symbol_counts: Vec<(String, usize)> = sym_stmt
        .query_map(rusqlite::params![param], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Dependency count.
    let mut dep_stmt = conn.prepare_cached(dep_query)?;
    let dep_count: i64 = dep_stmt.query_row(rusqlite::params![param], |row| row.get(0))?;

    Ok(SummaryMetrics {
        file_count,
        line_count,
        symbol_counts,
        language_breakdown,
        dependency_count: dep_count as usize,
    })
}

/// Check whether we should recurse at the given current depth.
fn should_recurse(max_depth: Option<usize>, current_depth: usize) -> bool {
    if current_depth >= MAX_RECURSIVE_DEPTH {
        return false;
    }
    match max_depth {
        None => true,
        Some(d) => current_depth < d,
    }
}

/// Pre-fetched subtree data for fully in-memory metric aggregation.
/// Loaded once with 4 SQL queries; all recursive child building uses this
/// data with zero additional SQL.
struct SubtreeData {
    /// All file paths under the subtree, sorted.
    paths: Vec<String>,
    /// (path, language, line_count) for every file in the subtree.
    file_rows: Vec<(String, String, usize)>,
    /// (file, kind) for every symbol in the subtree.
    symbol_rows: Vec<(String, String)>,
    /// (source_file, import_path) for every import in the subtree.
    import_rows: Vec<(String, String)>,
}

impl SubtreeData {
    /// Load all subtree data in exactly 4 SQL queries.
    fn load(conn: &Connection, like_pattern: &str) -> Result<Self> {
        let mut path_stmt = conn.prepare_cached(
            "SELECT DISTINCT path FROM files WHERE path LIKE ?1 ESCAPE '\\' ORDER BY path",
        )?;
        let paths: Vec<String> = path_stmt
            .query_map(rusqlite::params![like_pattern], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut file_stmt = conn.prepare_cached(
            "SELECT path, language, COALESCE(line_count, 0) \
             FROM files WHERE path LIKE ?1 ESCAPE '\\'",
        )?;
        let file_rows: Vec<(String, String, usize)> = file_stmt
            .query_map(rusqlite::params![like_pattern], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as usize,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut sym_stmt =
            conn.prepare_cached("SELECT file, kind FROM symbols WHERE file LIKE ?1 ESCAPE '\\'")?;
        let symbol_rows: Vec<(String, String)> = sym_stmt
            .query_map(rusqlite::params![like_pattern], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut imp_stmt = conn.prepare_cached(
            "SELECT source_file, import_path FROM file_imports \
             WHERE source_file LIKE ?1 ESCAPE '\\'",
        )?;
        let import_rows: Vec<(String, String)> = imp_stmt
            .query_map(rusqlite::params![like_pattern], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            paths,
            file_rows,
            symbol_rows,
            import_rows,
        })
    }

    /// Compute aggregated metrics for all paths matching a prefix, in memory.
    fn metrics_for_prefix(&self, prefix: &str, is_file: bool) -> SummaryMetrics {
        use std::collections::{BTreeMap, HashSet};

        let mut file_count = 0usize;
        let mut line_count = 0usize;
        let mut lang_map: BTreeMap<String, usize> = BTreeMap::new();

        for (path, lang, lines) in &self.file_rows {
            if (is_file && path == prefix) || (!is_file && path.starts_with(prefix)) {
                file_count += 1;
                line_count += lines;
                *lang_map.entry(lang.clone()).or_default() += 1;
            }
        }

        let mut sym_map: BTreeMap<String, usize> = BTreeMap::new();
        for (file, kind) in &self.symbol_rows {
            if (is_file && file == prefix) || (!is_file && file.starts_with(prefix)) {
                *sym_map.entry(kind.clone()).or_default() += 1;
            }
        }

        let mut dep_set: HashSet<&str> = HashSet::new();
        for (source_file, import_path) in &self.import_rows {
            if (is_file && source_file == prefix) || (!is_file && source_file.starts_with(prefix)) {
                dep_set.insert(import_path);
            }
        }

        SummaryMetrics {
            file_count,
            line_count,
            symbol_counts: sym_map.into_iter().collect(),
            language_breakdown: lang_map.into_iter().collect(),
            dependency_count: dep_set.len(),
        }
    }
}

/// Find the subslice of sorted paths that start with the given prefix,
/// using binary search for O(log N) instead of scanning the full list.
fn paths_with_prefix<'a>(sorted_paths: &'a [String], prefix: &str) -> &'a [String] {
    let start = sorted_paths.partition_point(|p| p.as_str() < prefix);
    let end = sorted_paths[start..].partition_point(|p| p.starts_with(prefix)) + start;
    &sorted_paths[start..end]
}

/// Build children from pre-fetched subtree data entirely in memory.
/// After SubtreeData::load() runs 4 SQL queries at the top level, this
/// function and its recursion perform zero additional SQL.
fn build_children_from_data(
    data: &SubtreeData,
    dir_prefix: &str,
    options: &SummaryOptions,
    current_depth: usize,
) -> Result<Vec<SummaryResult>> {
    let subtree = paths_with_prefix(&data.paths, dir_prefix);
    let prefix_len = dir_prefix.len();

    // Extract unique immediate children from the narrowed slice.
    let mut seen = std::collections::HashSet::with_capacity(subtree.len());
    let mut child_entries: Vec<(String, SummaryPathType)> = Vec::with_capacity(subtree.len());

    for path in subtree {
        let suffix = &path[prefix_len..];
        if let Some(slash_pos) = suffix.find('/') {
            let dir_name = &suffix[..=slash_pos];
            let full_child = format!("{dir_prefix}{dir_name}");
            if seen.insert(full_child.clone()) {
                child_entries.push((full_child, SummaryPathType::Directory));
            }
        } else if seen.insert(path.clone()) {
            child_entries.push((path.clone(), SummaryPathType::File));
        }
    }

    let mut results = Vec::with_capacity(child_entries.len());
    for (child_path, child_type) in &child_entries {
        let is_file = *child_type == SummaryPathType::File;
        let metrics = data.metrics_for_prefix(child_path, is_file);

        let grandchildren = if !is_file && should_recurse(options.depth, current_depth) {
            build_children_from_data(data, child_path, options, current_depth + 1)?
        } else {
            vec![]
        };

        results.push(SummaryResult {
            path: child_path.clone(),
            path_type: *child_type,
            detail_level: options.detail,
            metrics,
            children: grandchildren,
            description: None,
        });
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::pipeline;
    use std::fs;
    use tempfile::TempDir;

    /// Create a minimal repo with the given files, index it, and return (TempDir, Connection).
    fn make_indexed_repo(files: &[(&str, &str)]) -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        fs::create_dir(root.join(".git")).unwrap();

        for (path, content) in files {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&full, content).unwrap();
        }

        pipeline::build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        (dir, conn)
    }

    fn default_options() -> SummaryOptions {
        SummaryOptions {
            detail: DetailLevel::Rich,
            depth: Some(0),
            suppress: true,
        }
    }

    #[test]
    fn summary_single_file() {
        let source = "fn hello() {\n    println!(\"hi\");\n}\nfn world() {}\n";
        let (dir, conn) = make_indexed_repo(&[("src/lib.rs", source)]);

        let result = summarize_path(&conn, "src/lib.rs", dir.path(), &default_options()).unwrap();

        assert_eq!(result.path, "src/lib.rs");
        assert_eq!(result.path_type, SummaryPathType::File);
        assert_eq!(result.metrics.file_count, 1);
        assert!(result.metrics.line_count > 0);
        assert!(!result.metrics.symbol_counts.is_empty());
        assert!(result.children.is_empty());
    }

    #[test]
    fn summary_directory() {
        let (dir, conn) = make_indexed_repo(&[
            ("src/a.rs", "fn alpha() {}\n"),
            ("src/b.rs", "fn beta() {}\n"),
        ]);

        let result = summarize_path(&conn, "src", dir.path(), &default_options()).unwrap();

        assert_eq!(result.path_type, SummaryPathType::Directory);
        assert_eq!(result.metrics.file_count, 2);
        // Should have function symbols from both files.
        let total_syms: usize = result.metrics.symbol_counts.iter().map(|(_, c)| c).sum();
        assert!(total_syms >= 2);
    }

    #[test]
    fn summary_empty_path_returns_zero_metrics() {
        let (dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);

        let result = summarize_path(&conn, "nonexistent", dir.path(), &default_options()).unwrap();

        assert_eq!(result.metrics.file_count, 0);
        assert_eq!(result.metrics.line_count, 0);
        assert!(result.metrics.symbol_counts.is_empty());
    }

    #[test]
    fn summary_directory_does_not_match_prefix_files() {
        // "src" should NOT match "src_utils.rs"
        let (dir, conn) = make_indexed_repo(&[
            ("src/lib.rs", "fn hello() {}\n"),
            ("src_utils.rs", "fn util() {}\n"),
        ]);

        let result = summarize_path(&conn, "src", dir.path(), &default_options()).unwrap();

        assert_eq!(result.metrics.file_count, 1);
    }

    #[test]
    fn summary_depth_one_shows_children() {
        let (dir, conn) = make_indexed_repo(&[
            ("src/a.rs", "fn alpha() {}\n"),
            ("src/sub/b.rs", "fn beta() {}\n"),
        ]);

        let opts = SummaryOptions {
            depth: Some(1),
            ..default_options()
        };
        let result = summarize_path(&conn, "src", dir.path(), &opts).unwrap();

        assert!(!result.children.is_empty());
        let child_paths: Vec<&str> = result.children.iter().map(|c| c.path.as_str()).collect();
        assert!(child_paths.contains(&"src/a.rs"));
        assert!(child_paths.contains(&"src/sub/"));
    }

    #[test]
    fn summary_depth_zero_no_children() {
        let (dir, conn) = make_indexed_repo(&[
            ("src/a.rs", "fn alpha() {}\n"),
            ("src/b.rs", "fn beta() {}\n"),
        ]);

        let opts = SummaryOptions {
            depth: Some(0),
            ..default_options()
        };
        let result = summarize_path(&conn, "src", dir.path(), &opts).unwrap();

        assert!(result.children.is_empty());
    }

    #[test]
    fn summary_unlimited_depth() {
        let (dir, conn) = make_indexed_repo(&[
            ("src/a.rs", "fn alpha() {}\n"),
            ("src/sub/b.rs", "fn beta() {}\n"),
            ("src/sub/deep/c.rs", "fn gamma() {}\n"),
        ]);

        let opts = SummaryOptions {
            depth: None, // unlimited
            ..default_options()
        };
        let result = summarize_path(&conn, "src", dir.path(), &opts).unwrap();

        // Should have children, and sub/ should have grandchildren.
        assert!(!result.children.is_empty());
        let sub_child = result
            .children
            .iter()
            .find(|c| c.path == "src/sub/")
            .expect("should find src/sub/");
        assert!(!sub_child.children.is_empty());
    }

    #[test]
    fn summary_multi_language() {
        let (dir, conn) = make_indexed_repo(&[
            ("src/lib.rs", "fn hello() {}\n"),
            ("src/main.py", "def world():\n    pass\n"),
        ]);

        let result = summarize_path(&conn, "src", dir.path(), &default_options()).unwrap();

        assert!(result.metrics.language_breakdown.len() >= 2);
    }

    #[test]
    fn summary_dependency_count() {
        // Create a JS file with imports that the indexer will pick up.
        let js_source =
            "import { foo } from './bar';\nimport { baz } from './qux';\nfunction main() {}\n";
        let (dir, conn) = make_indexed_repo(&[
            ("src/app.js", js_source),
            ("src/bar.js", "export function foo() {}\n"),
            ("src/qux.js", "export function baz() {}\n"),
        ]);

        let result = summarize_path(&conn, "src/app.js", dir.path(), &default_options()).unwrap();

        // app.js imports from bar and qux.
        assert!(result.metrics.dependency_count >= 2);
    }

    #[test]
    fn summary_detail_level_propagated() {
        let (dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);

        for level in [DetailLevel::Rich, DetailLevel::Light, DetailLevel::Symbols] {
            let opts = SummaryOptions {
                detail: level,
                ..default_options()
            };
            let result = summarize_path(&conn, "src", dir.path(), &opts).unwrap();
            assert_eq!(result.detail_level, level);
        }
    }

    #[test]
    fn summary_normalize_path() {
        assert_eq!(normalize_path("./src/"), "src");
        assert_eq!(normalize_path("./src"), "src");
        assert_eq!(normalize_path("src/"), "src");
        assert_eq!(normalize_path("src"), "src");
        assert_eq!(normalize_path("./"), ".");
        assert_eq!(normalize_path("."), ".");
        assert_eq!(normalize_path("src/lib.rs"), "src/lib.rs");
    }

    #[test]
    fn summary_with_trailing_slash() {
        let (dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);

        let result = summarize_path(&conn, "src/", dir.path(), &default_options()).unwrap();

        assert_eq!(result.path_type, SummaryPathType::Directory);
        assert_eq!(result.metrics.file_count, 1);
    }

    #[test]
    fn summary_with_dot_slash_prefix() {
        let (dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);

        let result = summarize_path(&conn, "./src", dir.path(), &default_options()).unwrap();

        assert_eq!(result.path_type, SummaryPathType::Directory);
        assert_eq!(result.metrics.file_count, 1);
    }
}
