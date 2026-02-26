//! Source body retrieval for `wonk show`.
//!
//! Queries the symbol index and reads actual source files on disk to extract
//! function/class bodies between `line` and `end_line`. Falls back to the
//! stored `signature` when `end_line` is not available.

use std::path::Path;
use std::str::FromStr;

use anyhow::Result;
use rusqlite::Connection;

use crate::types::{ShowResult, SymbolKind};

/// Options for filtering `show` results.
pub struct ShowOptions {
    /// Restrict results to a specific file path (substring match).
    pub file: Option<String>,
    /// Restrict results to a specific symbol kind.
    pub kind: Option<String>,
    /// Require exact name match (default: substring / LIKE).
    pub exact: bool,
}

/// Query the index for symbols matching `name` and read their source bodies
/// from disk.
///
/// Returns one [`ShowResult`] per matched symbol, ordered by file then line.
/// Symbols whose source file no longer exists on disk are silently skipped
/// with a warning to stderr.
pub fn show_symbol(
    conn: &Connection,
    name: &str,
    repo_root: &Path,
    options: &ShowOptions,
) -> Result<Vec<ShowResult>> {
    // Build dynamic SQL with optional filters.
    let mut sql = String::from(
        "SELECT name, kind, file, line, end_line, signature, language \
         FROM symbols WHERE ",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if options.exact {
        sql.push_str("name = ?");
        params.push(Box::new(name.to_string()));
    } else {
        sql.push_str("name LIKE ?");
        params.push(Box::new(format!("%{name}%")));
    }

    if let Some(ref kind_str) = options.kind {
        // Validate kind early so we get a clear error.
        let _kind = SymbolKind::from_str(kind_str).map_err(|e| anyhow::anyhow!("{e}"))?;
        sql.push_str(" AND kind = ?");
        params.push(Box::new(kind_str.clone()));
    }

    if let Some(ref file_filter) = options.file {
        sql.push_str(" AND file LIKE ?");
        params.push(Box::new(format!("%{file_filter}%")));
    }

    sql.push_str(" ORDER BY file, line");

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(param_refs), |row| {
        let kind_str: String = row.get(1)?;
        Ok((
            row.get::<_, String>(0)?, // name
            kind_str,
            row.get::<_, String>(2)?,      // file
            row.get::<_, i64>(3)?,         // line
            row.get::<_, Option<i64>>(4)?, // end_line
            row.get::<_, String>(5)?,      // signature
            row.get::<_, String>(6)?,      // language
        ))
    })?;

    let mut results = Vec::new();

    for row in rows {
        let (sym_name, kind_str, file, line, end_line, signature, language) = row?;
        let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
        let line = line as usize;
        let end_line = end_line.map(|v| v as usize);

        // Read source body from disk.
        let source = if let Some(end) = end_line {
            let abs_path = repo_root.join(&file);
            match std::fs::read_to_string(&abs_path) {
                Ok(content) => extract_lines(&content, line, end),
                Err(_) => {
                    eprintln!("warning: source file not found: {file}");
                    continue;
                }
            }
        } else {
            // No end_line: fall back to signature.
            signature
        };

        results.push(ShowResult {
            name: sym_name,
            kind,
            file,
            line,
            end_line,
            source,
            language,
        });
    }

    Ok(results)
}

/// Extract lines `start..=end` (1-based) from content.
fn extract_lines(content: &str, start: usize, end: usize) -> String {
    content
        .lines()
        .enumerate()
        .filter(|(i, _)| {
            let line_no = i + 1;
            line_no >= start && line_no <= end
        })
        .map(|(_, line)| line)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::pipeline;
    use std::fs;
    use tempfile::TempDir;

    /// Create a minimal Rust repo, index it, and return (TempDir, Connection).
    fn make_indexed_repo(source: &str) -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), source).unwrap();

        pipeline::build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        (dir, conn)
    }

    fn default_options() -> ShowOptions {
        ShowOptions {
            file: None,
            kind: None,
            exact: false,
        }
    }

    #[test]
    fn basic_function_show() {
        let source = "fn hello() {\n    println!(\"hi\");\n}\n";
        let (dir, conn) = make_indexed_repo(source);

        let results = show_symbol(&conn, "hello", dir.path(), &default_options()).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "hello");
        assert_eq!(results[0].kind, SymbolKind::Function);
        assert!(results[0].source.contains("println!"));
    }

    #[test]
    fn exact_match_filters_correctly() {
        let source = "fn hello() { }\nfn hello_world() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        let opts = ShowOptions {
            exact: true,
            ..default_options()
        };
        let results = show_symbol(&conn, "hello", dir.path(), &opts).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "hello");
    }

    #[test]
    fn substring_match_finds_multiple() {
        let source = "fn hello() { }\nfn hello_world() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        let results = show_symbol(&conn, "hello", dir.path(), &default_options()).unwrap();

        assert_eq!(results.len(), 2);
    }

    #[test]
    fn kind_filter() {
        let source = "const HELLO: i32 = 1;\nfn hello() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        let opts = ShowOptions {
            kind: Some("function".into()),
            ..default_options()
        };
        let results = show_symbol(&conn, "hello", dir.path(), &opts).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].kind, SymbolKind::Function);
    }

    #[test]
    fn file_filter() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn hello() { }\n").unwrap();
        fs::write(root.join("src/other.rs"), "fn hello() { }\n").unwrap();

        pipeline::build_index(root, true).unwrap();
        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        let opts = ShowOptions {
            file: Some("other.rs".into()),
            ..default_options()
        };
        let results = show_symbol(&conn, "hello", root, &opts).unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].file.contains("other.rs"));
    }

    #[test]
    fn missing_source_file_skipped() {
        let source = "fn hello() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        // Remove the source file after indexing.
        fs::remove_file(dir.path().join("src/lib.rs")).unwrap();

        let results = show_symbol(&conn, "hello", dir.path(), &default_options()).unwrap();

        // Symbol should be skipped (source file missing).
        assert!(results.is_empty());
    }

    #[test]
    fn no_end_line_fallback_to_signature() {
        // Constants in Rust don't get end_line in tree-sitter, so they'll
        // use signature fallback. We can also test by inserting directly.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "const MAX: usize = 1024;\n").unwrap();

        pipeline::build_index(root, true).unwrap();
        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        let results = show_symbol(&conn, "MAX", root, &default_options()).unwrap();

        assert_eq!(results.len(), 1);
        // Should fall back to signature since constants have no end_line.
        assert!(!results[0].source.is_empty());
    }

    #[test]
    fn no_matches_returns_empty() {
        let source = "fn hello() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        let results = show_symbol(&conn, "nonexistent", dir.path(), &default_options()).unwrap();

        assert!(results.is_empty());
    }

    #[test]
    fn extract_lines_basic() {
        let content = "line1\nline2\nline3\nline4\nline5\n";
        assert_eq!(extract_lines(content, 2, 4), "line2\nline3\nline4");
    }

    #[test]
    fn extract_lines_single() {
        let content = "line1\nline2\nline3\n";
        assert_eq!(extract_lines(content, 2, 2), "line2");
    }
}
