//! Structural summary engine for `wonk summary`.
//!
//! Queries the SQLite index to aggregate structural metrics (file count, line
//! count, symbol counts by kind, language breakdown, dependency count) for a
//! given path. Supports three detail levels and recursive depth traversal.

use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

use crate::types::{DetailLevel, SummaryMetrics, SummaryPathType, SummaryResult};

/// Options for the summary engine.
pub struct SummaryOptions {
    /// Detail level for the output.
    pub detail: DetailLevel,
    /// Maximum recursion depth. `None` means unlimited.
    pub depth: Option<usize>,
    /// Whether to suppress stderr hints.
    pub suppress: bool,
}

/// Summarize a path (file or directory) using the index.
///
/// The path is relative to the repo root. Directories use `prefix/` LIKE
/// patterns to avoid false matches (e.g. `src` won't match `src_utils.rs`).
/// Files use exact match.
///
/// Returns a `SummaryResult` with zero metrics for empty/unknown paths
/// (not an error), consistent with other wonk commands.
pub fn summarize_path(
    conn: &Connection,
    path: &str,
    _repo_root: &Path,
    options: &SummaryOptions,
) -> Result<SummaryResult> {
    // Normalize path: strip leading `./`, ensure directories end with `/`.
    let normalized = normalize_path(path);

    // Determine if this is a file or directory by checking the DB.
    let path_type = detect_path_type(conn, &normalized)?;

    // Build the LIKE pattern.
    let (like_pattern, exact_path) = match path_type {
        SummaryPathType::File => (normalized.clone(), normalized.clone()),
        SummaryPathType::Directory => {
            let prefix = if normalized.ends_with('/') {
                normalized.clone()
            } else {
                format!("{normalized}/")
            };
            (format!("{prefix}%"), prefix)
        }
    };

    let metrics = query_metrics(conn, &like_pattern, &exact_path, path_type)?;

    // Recurse into children if depth allows.
    let children = if should_recurse(options.depth, 0) && path_type == SummaryPathType::Directory {
        enumerate_children(conn, &exact_path, options, 1)?
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
    let mut p = path.to_string();
    // Strip leading `./`
    while p.starts_with("./") {
        p = p[2..].to_string();
    }
    // Strip trailing `/` for consistent handling; we'll add it back for directories.
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    if p.is_empty() { ".".to_string() } else { p }
}

/// Check whether a path refers to a file or directory in the index.
fn detect_path_type(conn: &Connection, path: &str) -> Result<SummaryPathType> {
    // Check for exact file match first.
    let file_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files WHERE path = ?1",
        rusqlite::params![path],
        |row| row.get(0),
    )?;

    if file_count > 0 {
        return Ok(SummaryPathType::File);
    }

    // Otherwise treat as directory.
    Ok(SummaryPathType::Directory)
}

/// Query aggregated metrics for a path.
fn query_metrics(
    conn: &Connection,
    like_pattern: &str,
    exact_path: &str,
    path_type: SummaryPathType,
) -> Result<SummaryMetrics> {
    // Files and lines, grouped by language.
    let mut lang_stmt = conn.prepare(
        "SELECT language, COUNT(*), COALESCE(SUM(line_count), 0) \
         FROM files WHERE path LIKE ?1 OR path = ?2 \
         GROUP BY language ORDER BY language",
    )?;

    let lang_rows: Vec<(String, usize, usize)> = lang_stmt
        .query_map(rusqlite::params![like_pattern, exact_path], |row| {
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
    let mut sym_stmt = conn.prepare(
        "SELECT kind, COUNT(*) FROM symbols \
         WHERE file LIKE ?1 OR file = ?2 \
         GROUP BY kind ORDER BY kind",
    )?;

    let (sym_like, sym_exact) = match path_type {
        SummaryPathType::File => (exact_path.to_string(), exact_path.to_string()),
        SummaryPathType::Directory => (like_pattern.to_string(), exact_path.to_string()),
    };

    let symbol_counts: Vec<(String, usize)> = sym_stmt
        .query_map(rusqlite::params![sym_like, sym_exact], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Dependency count.
    let dep_count: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT import_path) FROM file_imports \
         WHERE source_file LIKE ?1 OR source_file = ?2",
        rusqlite::params![like_pattern, exact_path],
        |row| row.get(0),
    )?;

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
    match max_depth {
        None => true, // Unlimited
        Some(d) => current_depth < d,
    }
}

/// Enumerate immediate children of a directory prefix from the files table
/// and recursively summarize each.
fn enumerate_children(
    conn: &Connection,
    dir_prefix: &str,
    options: &SummaryOptions,
    current_depth: usize,
) -> Result<Vec<SummaryResult>> {
    // Find distinct immediate children (files and subdirectories).
    // For paths like "src/foo.rs" under prefix "src/", the immediate child is "foo.rs".
    // For paths like "src/bar/baz.rs" under prefix "src/", the immediate child is "bar/".
    let prefix_len = dir_prefix.len();

    let mut stmt =
        conn.prepare("SELECT DISTINCT path FROM files WHERE path LIKE ?1 ORDER BY path")?;

    let like_pattern = format!("{dir_prefix}%");
    let paths: Vec<String> = stmt
        .query_map(rusqlite::params![like_pattern], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Extract unique immediate children.
    let mut seen = std::collections::HashSet::new();
    let mut child_paths: Vec<(String, SummaryPathType)> = Vec::new();

    for path in &paths {
        let suffix = &path[prefix_len..];
        if let Some(slash_pos) = suffix.find('/') {
            // Subdirectory: take up to the slash.
            let dir_name = &suffix[..=slash_pos];
            let full_child = format!("{dir_prefix}{dir_name}");
            if seen.insert(full_child.clone()) {
                child_paths.push((full_child, SummaryPathType::Directory));
            }
        } else {
            // Direct file.
            if seen.insert(path.clone()) {
                child_paths.push((path.clone(), SummaryPathType::File));
            }
        }
    }

    // Summarize each child.
    let mut results = Vec::new();
    for (child_path, child_type) in &child_paths {
        let (child_like, child_exact) = match child_type {
            SummaryPathType::File => (child_path.clone(), child_path.clone()),
            SummaryPathType::Directory => (format!("{child_path}%"), child_path.clone()),
        };

        let metrics = query_metrics(conn, &child_like, &child_exact, *child_type)?;

        let grandchildren = if *child_type == SummaryPathType::Directory
            && should_recurse(options.depth, current_depth)
        {
            enumerate_children(conn, child_path, options, current_depth + 1)?
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
