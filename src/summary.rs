//! Structural summary engine for `wonk summary`.
//!
//! Queries the SQLite index to aggregate structural metrics (file count, line
//! count, symbol counts by kind, language breakdown, dependency count) for a
//! given path. Supports three detail levels and recursive depth traversal.

use anyhow::Result;
use rusqlite::Connection;

use crate::config::LlmConfig;
use crate::types::{
    DetailLevel, ImportEdge, SummaryMetrics, SummaryPathType, SummaryResult, SummarySymbol,
};

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
    /// LLM config for `--semantic` descriptions.
    /// `None` means structural only; `Some` triggers LLM generation.
    pub semantic: Option<LlmConfig>,
    /// When true, include all symbols (including scoped) for tree display.
    pub tree: bool,
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
pub fn summarize_path(
    conn: &Connection,
    path: &str,
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

    // Pre-fetch subtree data when we need children or rich directory details.
    let need_subtree = path_type == SummaryPathType::Directory
        && (should_recurse(options.depth, 0) || options.detail == DetailLevel::Rich);
    let subtree_data = if need_subtree {
        Some(SubtreeData::load(conn, &like_pattern)?)
    } else {
        None
    };

    // Recurse into children if depth allows.
    let children = if should_recurse(options.depth, 0) && path_type == SummaryPathType::Directory {
        build_children_from_data(subtree_data.as_ref().unwrap(), &exact_path, options, 1)?
    } else {
        vec![]
    };

    // Generate LLM description (top-level only, not per-child).
    let description = if let Some(ref llm_config) = options.semantic {
        generate_description(
            conn,
            &normalized,
            &like_pattern,
            path_type,
            &metrics,
            llm_config,
            options.suppress,
        )
    } else {
        None
    };

    // For Rich detail on directories, compute intra-directory import edges.
    let import_edges = if options.detail == DetailLevel::Rich {
        if let Some(ref data) = subtree_data {
            data.import_edges_for_dir(&exact_path)
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    // For Rich detail on files, include symbols.
    let symbols = if options.detail == DetailLevel::Rich && path_type == SummaryPathType::File {
        symbols_for_file(conn, &normalized, options.tree)?
    } else {
        vec![]
    };

    Ok(SummaryResult {
        path: normalized,
        path_type,
        detail_level: options.detail,
        metrics,
        children,
        description,
        symbols,
        import_edges,
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

/// (file, kind, name, signature, line, col, end_line, scope) per symbol.
type SymbolRow = (
    String,
    String,
    String,
    String,
    usize,
    usize,
    Option<usize>,
    Option<String>,
);

/// Pre-fetched subtree data for fully in-memory metric aggregation.
/// Loaded once with 4 SQL queries; all recursive child building uses this
/// data with zero additional SQL.
struct SubtreeData {
    /// All file paths under the subtree, sorted.
    paths: Vec<String>,
    /// (path, language, line_count) for every file in the subtree.
    file_rows: Vec<(String, String, usize)>,
    /// (file, kind, name, signature, line, col, end_line, scope) for every symbol in the subtree.
    symbol_rows: Vec<SymbolRow>,
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

        let mut sym_stmt = conn.prepare_cached(
            "SELECT file, kind, name, COALESCE(signature, ''), line, col, end_line, scope \
             FROM symbols WHERE file LIKE ?1 ESCAPE '\\'",
        )?;
        let symbol_rows: Vec<SymbolRow> = sym_stmt
            .query_map(rusqlite::params![like_pattern], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)? as usize,
                    row.get::<_, i64>(5)? as usize,
                    row.get::<_, Option<i64>>(6)?.map(|v| v as usize),
                    row.get::<_, Option<String>>(7)?,
                ))
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
        for (file, kind, _, _, _, _, _, _) in &self.symbol_rows {
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

    /// Return symbols for a specific file.
    /// When `tree` is false, returns only top-level symbols (scope IS NULL), capped at 50.
    /// When `tree` is true, returns ALL symbols (no scope filter or cap).
    fn symbols_for_file(&self, file: &str, tree: bool) -> Vec<SummarySymbol> {
        let iter = self
            .symbol_rows
            .iter()
            .filter(|(f, _, _, _, _, _, _, _)| f == file);
        if tree {
            iter.map(
                |(_, kind, name, sig, line, col, end_line, scope)| SummarySymbol {
                    name: name.clone(),
                    kind: kind.clone(),
                    signature: sig.clone(),
                    line: *line,
                    col: *col,
                    end_line: *end_line,
                    scope: scope.clone(),
                },
            )
            .collect()
        } else {
            iter.filter(|(_, _, _, _, _, _, _, scope)| scope.is_none())
                .take(50)
                .map(
                    |(_, kind, name, sig, line, col, end_line, _)| SummarySymbol {
                        name: name.clone(),
                        kind: kind.clone(),
                        signature: sig.clone(),
                        line: *line,
                        col: *col,
                        end_line: *end_line,
                        scope: None,
                    },
                )
                .collect()
        }
    }

    /// Return intra-directory import edges for files under `prefix`.
    ///
    /// For each import row where source starts with prefix, stem-match the
    /// import_path against files within the prefix directory.
    fn import_edges_for_dir(&self, prefix: &str) -> Vec<ImportEdge> {
        use std::collections::{HashMap, HashSet};
        use std::path::Path;

        // Build a stem → path lookup for files in this directory.
        let mut stem_map: HashMap<String, Vec<&str>> = HashMap::new();
        for path in &self.paths {
            if path.starts_with(prefix)
                && let Some(stem) = Path::new(path.as_str())
                    .file_stem()
                    .and_then(|s| s.to_str())
            {
                stem_map
                    .entry(stem.to_string())
                    .or_default()
                    .push(path.as_str());
            }
        }

        let mut seen = HashSet::new();
        let mut edges = Vec::new();

        for (source_file, import_path) in &self.import_rows {
            if !source_file.starts_with(prefix) {
                continue;
            }
            // Extract stem from import path (e.g. "./bar" → "bar", "../utils" → "utils").
            let import_stem = Path::new(import_path.as_str())
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(import_path.as_str());

            if let Some(targets) = stem_map.get(import_stem) {
                for &target in targets {
                    if target != source_file.as_str()
                        && seen.insert((source_file.clone(), target.to_string()))
                    {
                        edges.push(ImportEdge {
                            from: source_file.clone(),
                            to: target.to_string(),
                        });
                    }
                }
            }
        }
        edges
    }
}

/// Query symbols for a single file directly from the DB.
/// Used when summarizing a file at the top level (no SubtreeData loaded).
/// When `tree` is false, returns only top-level symbols (scope IS NULL), capped at 50.
/// When `tree` is true, returns ALL symbols (no scope filter or cap).
fn symbols_for_file(conn: &Connection, file: &str, tree: bool) -> Result<Vec<SummarySymbol>> {
    let sql = if tree {
        "SELECT kind, name, COALESCE(signature, ''), line, col, end_line, scope \
         FROM symbols WHERE file = ?1"
    } else {
        "SELECT kind, name, COALESCE(signature, ''), line, col, end_line, scope \
         FROM symbols WHERE file = ?1 AND scope IS NULL LIMIT 50"
    };
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt
        .query_map(rusqlite::params![file], |row| {
            Ok(SummarySymbol {
                kind: row.get::<_, String>(0)?,
                name: row.get::<_, String>(1)?,
                signature: row.get::<_, String>(2)?,
                line: row.get::<_, i64>(3)? as usize,
                col: row.get::<_, i64>(4)? as usize,
                end_line: row.get::<_, Option<i64>>(5)?.map(|v| v as usize),
                scope: row.get::<_, Option<String>>(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
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

        // For Rich detail: file children get symbols,
        // directory children get intra-directory import edges.
        let (symbols, import_edges) = if options.detail == DetailLevel::Rich {
            if is_file {
                (data.symbols_for_file(child_path, options.tree), vec![])
            } else {
                (vec![], data.import_edges_for_dir(child_path))
            }
        } else {
            (vec![], vec![])
        };

        results.push(SummaryResult {
            path: child_path.clone(),
            path_type: *child_type,
            detail_level: options.detail,
            metrics,
            children: grandchildren,
            description: None,
            symbols,
            import_edges,
        });
    }

    Ok(results)
}

/// Attempt to generate an LLM description for the given path.
///
/// Returns `Some(description)` on success or cache hit, `None` if Ollama is
/// unreachable (with a stderr warning), or `None` on other errors.
fn generate_description(
    conn: &Connection,
    path: &str,
    like_pattern: &str,
    path_type: SummaryPathType,
    metrics: &SummaryMetrics,
    config: &LlmConfig,
    suppress: bool,
) -> Option<String> {
    use crate::errors::LlmError;
    use crate::llm;
    use crate::output;

    // 1. Compute content hash.
    let content_hash = match llm::compute_content_hash(conn, like_pattern, path_type) {
        Ok(h) => h,
        Err(e) => {
            output::print_hint(&format!("failed to compute content hash: {e}"), suppress);
            return None;
        }
    };

    // 2. Check cache.
    if let Some(cached) = llm::get_cached(conn, path, &content_hash) {
        return Some(cached);
    }

    // 3. Build prompt.
    let prompt = match llm::build_prompt(conn, path, like_pattern, path_type, metrics) {
        Ok(p) => p,
        Err(e) => {
            output::print_hint(&format!("failed to build prompt: {e}"), suppress);
            return None;
        }
    };

    // 4. Call Ollama generate.
    match llm::generate(config, &prompt) {
        Ok(description) => {
            // Store in cache (ignore cache write errors).
            let _ = llm::store_cache(conn, path, &content_hash, &description);
            Some(description)
        }
        Err(LlmError::OllamaUnreachable) => {
            output::print_hint(
                "Ollama is not reachable; returning structural summary only",
                suppress,
            );
            None
        }
        Err(LlmError::ModelNotFound(model)) => {
            output::print_error(&format!(
                "model '{model}' not found; run `ollama pull {model}` \
                 or configure [llm].model in .wonk/config.toml"
            ));
            None
        }
        Err(e) => {
            output::print_hint(&format!("LLM generation failed: {e}"), suppress);
            None
        }
    }
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
            semantic: None,
            tree: false,
        }
    }

    #[test]
    fn summary_single_file() {
        let source = "fn hello() {\n    println!(\"hi\");\n}\nfn world() {}\n";
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", source)]);

        let result = summarize_path(&conn, "src/lib.rs", &default_options()).unwrap();

        assert_eq!(result.path, "src/lib.rs");
        assert_eq!(result.path_type, SummaryPathType::File);
        assert_eq!(result.metrics.file_count, 1);
        assert!(result.metrics.line_count > 0);
        assert!(!result.metrics.symbol_counts.is_empty());
        assert!(result.children.is_empty());
    }

    #[test]
    fn summary_directory() {
        let (_dir, conn) = make_indexed_repo(&[
            ("src/a.rs", "fn alpha() {}\n"),
            ("src/b.rs", "fn beta() {}\n"),
        ]);

        let result = summarize_path(&conn, "src", &default_options()).unwrap();

        assert_eq!(result.path_type, SummaryPathType::Directory);
        assert_eq!(result.metrics.file_count, 2);
        // Should have function symbols from both files.
        let total_syms: usize = result.metrics.symbol_counts.iter().map(|(_, c)| c).sum();
        assert!(total_syms >= 2);
    }

    #[test]
    fn summary_empty_path_returns_zero_metrics() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);

        let result = summarize_path(&conn, "nonexistent", &default_options()).unwrap();

        assert_eq!(result.metrics.file_count, 0);
        assert_eq!(result.metrics.line_count, 0);
        assert!(result.metrics.symbol_counts.is_empty());
    }

    #[test]
    fn summary_directory_does_not_match_prefix_files() {
        // "src" should NOT match "src_utils.rs"
        let (_dir, conn) = make_indexed_repo(&[
            ("src/lib.rs", "fn hello() {}\n"),
            ("src_utils.rs", "fn util() {}\n"),
        ]);

        let result = summarize_path(&conn, "src", &default_options()).unwrap();

        assert_eq!(result.metrics.file_count, 1);
    }

    #[test]
    fn summary_depth_one_shows_children() {
        let (_dir, conn) = make_indexed_repo(&[
            ("src/a.rs", "fn alpha() {}\n"),
            ("src/sub/b.rs", "fn beta() {}\n"),
        ]);

        let opts = SummaryOptions {
            depth: Some(1),
            ..default_options()
        };
        let result = summarize_path(&conn, "src", &opts).unwrap();

        assert!(!result.children.is_empty());
        let child_paths: Vec<&str> = result.children.iter().map(|c| c.path.as_str()).collect();
        assert!(child_paths.contains(&"src/a.rs"));
        assert!(child_paths.contains(&"src/sub/"));
    }

    #[test]
    fn summary_depth_zero_no_children() {
        let (_dir, conn) = make_indexed_repo(&[
            ("src/a.rs", "fn alpha() {}\n"),
            ("src/b.rs", "fn beta() {}\n"),
        ]);

        let opts = SummaryOptions {
            depth: Some(0),
            ..default_options()
        };
        let result = summarize_path(&conn, "src", &opts).unwrap();

        assert!(result.children.is_empty());
    }

    #[test]
    fn summary_unlimited_depth() {
        let (_dir, conn) = make_indexed_repo(&[
            ("src/a.rs", "fn alpha() {}\n"),
            ("src/sub/b.rs", "fn beta() {}\n"),
            ("src/sub/deep/c.rs", "fn gamma() {}\n"),
        ]);

        let opts = SummaryOptions {
            depth: None, // unlimited
            ..default_options()
        };
        let result = summarize_path(&conn, "src", &opts).unwrap();

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
        let (_dir, conn) = make_indexed_repo(&[
            ("src/lib.rs", "fn hello() {}\n"),
            ("src/main.py", "def world():\n    pass\n"),
        ]);

        let result = summarize_path(&conn, "src", &default_options()).unwrap();

        assert!(result.metrics.language_breakdown.len() >= 2);
    }

    #[test]
    fn summary_dependency_count() {
        // Create a JS file with imports that the indexer will pick up.
        let js_source =
            "import { foo } from './bar';\nimport { baz } from './qux';\nfunction main() {}\n";
        let (_dir, conn) = make_indexed_repo(&[
            ("src/app.js", js_source),
            ("src/bar.js", "export function foo() {}\n"),
            ("src/qux.js", "export function baz() {}\n"),
        ]);

        let result = summarize_path(&conn, "src/app.js", &default_options()).unwrap();

        // app.js imports from bar and qux.
        assert!(result.metrics.dependency_count >= 2);
    }

    #[test]
    fn summary_detail_level_propagated() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);

        for level in [DetailLevel::Rich, DetailLevel::Light, DetailLevel::Symbols] {
            let opts = SummaryOptions {
                detail: level,
                ..default_options()
            };
            let result = summarize_path(&conn, "src", &opts).unwrap();
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
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);

        let result = summarize_path(&conn, "src/", &default_options()).unwrap();

        assert_eq!(result.path_type, SummaryPathType::Directory);
        assert_eq!(result.metrics.file_count, 1);
    }

    #[test]
    fn summary_with_dot_slash_prefix() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);

        let result = summarize_path(&conn, "./src", &default_options()).unwrap();

        assert_eq!(result.path_type, SummaryPathType::Directory);
        assert_eq!(result.metrics.file_count, 1);
    }

    // -- Semantic (--semantic) tests -------------------------------------------

    #[test]
    fn summary_without_semantic_has_no_description() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);

        let opts = SummaryOptions {
            semantic: None,
            ..default_options()
        };
        let result = summarize_path(&conn, "src", &opts).unwrap();
        assert!(result.description.is_none());
    }

    #[test]
    fn summary_semantic_ollama_unreachable_returns_none_description() {
        // When Ollama is down, description should be None (graceful degradation).
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);
        crate::db::ensure_summaries_table(&conn).unwrap();

        let config = crate::config::LlmConfig {
            model: "llama3.2:3b".to_string(),
            generate_url: "http://127.0.0.1:19999/api/generate".to_string(),
        };
        let opts = SummaryOptions {
            semantic: Some(config),
            ..default_options()
        };
        let result = summarize_path(&conn, "src", &opts).unwrap();
        // Should succeed, but description is None since Ollama is unreachable.
        assert!(result.description.is_none());
    }

    #[test]
    fn summary_semantic_cached_description_returned() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);
        crate::db::ensure_summaries_table(&conn).unwrap();

        // Pre-populate the cache with the correct content hash.
        let hash =
            crate::llm::compute_content_hash(&conn, "src/%", SummaryPathType::Directory).unwrap();
        crate::llm::store_cache(&conn, "src", &hash, "Cached description.").unwrap();

        let config = crate::config::LlmConfig {
            model: "llama3.2:3b".to_string(),
            generate_url: "http://127.0.0.1:19999/api/generate".to_string(),
        };
        let opts = SummaryOptions {
            semantic: Some(config),
            ..default_options()
        };
        let result = summarize_path(&conn, "src", &opts).unwrap();
        assert_eq!(result.description, Some("Cached description.".to_string()));
    }

    #[test]
    fn summary_semantic_only_at_top_level() {
        // Children should NOT get LLM descriptions even when semantic is enabled.
        let (_dir, conn) = make_indexed_repo(&[
            ("src/a.rs", "fn alpha() {}\n"),
            ("src/sub/b.rs", "fn beta() {}\n"),
        ]);
        crate::db::ensure_summaries_table(&conn).unwrap();

        let config = crate::config::LlmConfig {
            model: "llama3.2:3b".to_string(),
            generate_url: "http://127.0.0.1:19999/api/generate".to_string(),
        };
        let opts = SummaryOptions {
            depth: Some(1),
            semantic: Some(config),
            ..default_options()
        };
        let result = summarize_path(&conn, "src", &opts).unwrap();

        // Children should have no description.
        for child in &result.children {
            assert!(
                child.description.is_none(),
                "child {} should not have description",
                child.path
            );
        }
    }

    #[test]
    fn summary_rich_depth1_includes_file_symbols() {
        let source = "fn alpha() {}\nfn beta(x: i32) -> bool { true }\n";
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", source)]);

        let opts = SummaryOptions {
            depth: Some(1),
            detail: DetailLevel::Rich,
            ..default_options()
        };
        let result = summarize_path(&conn, "src", &opts).unwrap();

        let file_child = result
            .children
            .iter()
            .find(|c| c.path == "src/lib.rs")
            .expect("should find src/lib.rs child");
        assert!(
            !file_child.symbols.is_empty(),
            "file child should have symbols"
        );
        let names: Vec<&str> = file_child.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "should contain alpha");
        assert!(names.contains(&"beta"), "should contain beta");
        // All should be functions.
        for s in &file_child.symbols {
            assert_eq!(s.kind, "function");
        }
    }

    #[test]
    fn summary_light_depth1_no_symbols() {
        let source = "fn alpha() {}\nfn beta() {}\n";
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", source)]);

        let opts = SummaryOptions {
            depth: Some(1),
            detail: DetailLevel::Light,
            ..default_options()
        };
        let result = summarize_path(&conn, "src", &opts).unwrap();

        for child in &result.children {
            assert!(
                child.symbols.is_empty(),
                "Light detail should not include symbols for {}",
                child.path
            );
        }
    }

    #[test]
    fn summary_rich_depth1_includes_import_edges() {
        let app_js = "import { foo } from './bar';\nfunction main() {}\n";
        let bar_js = "export function foo() {}\n";
        let (_dir, conn) = make_indexed_repo(&[("src/app.js", app_js), ("src/bar.js", bar_js)]);

        let opts = SummaryOptions {
            depth: Some(1),
            detail: DetailLevel::Rich,
            ..default_options()
        };
        let result = summarize_path(&conn, "src", &opts).unwrap();

        // Top-level directory should have import edges.
        assert!(!result.import_edges.is_empty(), "should have import edges");
        let edge = result
            .import_edges
            .iter()
            .find(|e| e.from == "src/app.js" && e.to == "src/bar.js");
        assert!(edge.is_some(), "should have edge from app.js to bar.js");
    }

    #[test]
    fn summary_symbols_are_toplevel_only() {
        // Python: top-level function + class with a method inside.
        let source =
            "def top_func():\n    pass\n\nclass MyClass:\n    def method(self):\n        pass\n";
        let (_dir, conn) = make_indexed_repo(&[("src/mod.py", source)]);

        let opts = SummaryOptions {
            depth: Some(1),
            detail: DetailLevel::Rich,
            ..default_options()
        };
        let result = summarize_path(&conn, "src", &opts).unwrap();

        let file_child = result
            .children
            .iter()
            .find(|c| c.path == "src/mod.py")
            .expect("should find src/mod.py");
        let names: Vec<&str> = file_child.symbols.iter().map(|s| s.name.as_str()).collect();
        // top_func and MyClass are top-level; method is scoped inside MyClass.
        assert!(names.contains(&"top_func"), "should contain top_func");
        assert!(names.contains(&"MyClass"), "should contain MyClass");
        assert!(
            !names.contains(&"method"),
            "should NOT contain scoped method"
        );
    }

    #[test]
    fn summary_file_toplevel_has_symbols() {
        // Summarizing a file directly (not as a child) should also include symbols.
        let source = "fn hello() {}\nfn world() {}\n";
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", source)]);

        let opts = SummaryOptions {
            depth: Some(0),
            detail: DetailLevel::Rich,
            ..default_options()
        };
        let result = summarize_path(&conn, "src/lib.rs", &opts).unwrap();

        assert!(
            !result.symbols.is_empty(),
            "file summary should have symbols"
        );
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"hello"));
        assert!(names.contains(&"world"));
    }
}
