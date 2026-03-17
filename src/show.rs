//! Source body retrieval for `wonk show`.
//!
//! Queries the symbol index and reads actual source files on disk to extract
//! function/class bodies between `line` and `end_line`. Falls back to the
//! stored `signature` when `end_line` is not available.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use anyhow::Result;
use rusqlite::Connection;

use crate::output;
use crate::types::{ShowResult, SymbolKind};

/// Options for filtering `show` results.
pub struct ShowOptions {
    /// Restrict results to a specific file path (substring match).
    pub file: Option<String>,
    /// Restrict results to a specific symbol kind.
    pub kind: Option<String>,
    /// Require exact name match (default: substring / LIKE).
    pub exact: bool,
    /// Whether to suppress stderr hints (--quiet or structured format).
    pub suppress: bool,
    /// Show container types in shallow mode (signature + child signatures only).
    pub shallow: bool,
    /// Restrict results to symbols with this scope (e.g. class name for methods).
    pub scope: Option<String>,
    /// Show only signatures for all symbols (no source bodies).
    pub signatures_only: bool,
}

/// Query all top-level symbols in a file (or directory prefix) and read their
/// source bodies from disk.
///
/// When `file_pattern` ends with `/`, it matches all files under that
/// directory. Otherwise it matches the exact file path.
pub fn show_file(
    conn: &Connection,
    file_pattern: &str,
    repo_root: &Path,
    options: &ShowOptions,
) -> Result<Vec<ShowResult>> {
    let like_pattern = if file_pattern.ends_with('/') {
        format!("{}%", escape_like(file_pattern))
    } else {
        escape_like(file_pattern)
    };

    let mut sql = String::from(
        "SELECT name, kind, file, line, end_line, signature, language \
         FROM symbols WHERE file LIKE ?1 ESCAPE '\\' AND scope IS NULL",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(like_pattern)];

    if let Some(ref kind_str) = options.kind {
        SymbolKind::from_str(kind_str).map_err(|e| anyhow::anyhow!("{e}"))?;
        sql.push_str(" AND kind = ?");
        params.push(Box::new(kind_str.clone()));
    }

    sql.push_str(" ORDER BY file, line");

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<_> = stmt
        .query_map(rusqlite::params_from_iter(param_refs), |row| {
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
        })?
        .collect::<Result<Vec<_>, _>>()?;

    collect_show_results(conn, rows, repo_root, options)
}

/// Query the index for symbols matching `name` and read their source bodies
/// from disk.
///
/// Returns one [`ShowResult`] per matched symbol, ordered by file then line.
/// Symbols whose source file no longer exists on disk are skipped with a
/// warning to stderr (PRD-SHOW-REQ-011).
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
        sql.push_str("name LIKE ? ESCAPE '\\'");
        params.push(Box::new(format!("%{}%", escape_like(name))));
    }

    if let Some(ref kind_str) = options.kind {
        // Validate kind early so we get a clear error.
        SymbolKind::from_str(kind_str).map_err(|e| anyhow::anyhow!("{e}"))?;
        sql.push_str(" AND kind = ?");
        params.push(Box::new(kind_str.clone()));
    }

    if let Some(ref file_filter) = options.file {
        sql.push_str(" AND LOWER(file) LIKE LOWER(?) ESCAPE '\\'");
        params.push(Box::new(format!("%{}%", escape_like(file_filter))));
    }

    if let Some(ref scope_filter) = options.scope {
        sql.push_str(" AND scope = ?");
        params.push(Box::new(scope_filter.clone()));
    }

    sql.push_str(" ORDER BY file, line");

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<_> = stmt
        .query_map(rusqlite::params_from_iter(param_refs), |row| {
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
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Prioritize exact name matches over substring matches, and deprioritize
    // test files, so the most relevant result appears first within budget.
    if !options.exact {
        let query_name = name.to_string();
        rows.sort_by(|a, b| {
            let a_exact = a.0.eq_ignore_ascii_case(&query_name);
            let b_exact = b.0.eq_ignore_ascii_case(&query_name);
            let a_test = is_test_path(&a.2);
            let b_test = is_test_path(&b.2);
            b_exact.cmp(&a_exact).then(a_test.cmp(&b_test))
        });
    }

    collect_show_results(conn, rows, repo_root, options)
}

/// Returns `true` for test/bench/spec file paths.
fn is_test_path(path: &str) -> bool {
    let p = path.to_lowercase();
    p.contains("test") || p.contains("spec") || p.contains("bench") || p.contains("example")
}

/// Shared logic: given queried rows, read source bodies and build `ShowResult`s.
type SymbolRow = (String, String, String, i64, Option<i64>, String, String);

fn collect_show_results(
    conn: &Connection,
    rows: Vec<SymbolRow>,
    repo_root: &Path,
    options: &ShowOptions,
) -> Result<Vec<ShowResult>> {
    let mut file_cache: HashMap<String, Option<String>> = HashMap::new();
    let canonical_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());

    let mut results = Vec::new();

    let mut child_stmt = if options.shallow {
        Some(conn.prepare(
            "SELECT signature FROM symbols WHERE scope = ?1 AND file = ?2 ORDER BY line",
        )?)
    } else {
        None
    };

    for (sym_name, kind_str, file, line, end_line, signature, language) in rows {
        let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
        let line = line as usize;
        let end_line = end_line.map(|v| v as usize);

        if options.signatures_only {
            let short_sig = signature.lines().next().unwrap_or(&signature).to_string();
            results.push(ShowResult {
                name: sym_name,
                kind,
                file,
                line,
                end_line,
                source: short_sig,
                language,
            });
            continue;
        }

        if options.shallow && kind.is_container() {
            let child_sigs =
                query_child_signatures_with_stmt(child_stmt.as_mut().unwrap(), &sym_name, &file)?;
            // Use only the first line of the container signature (the
            // class/struct declaration) — Python signatures can include
            // the entire docstring + __init__ body which defeats shallow mode.
            let short_sig = signature.lines().next().unwrap_or(&signature).to_string();
            let source = if child_sigs.is_empty() {
                short_sig
            } else {
                let mut parts = vec![short_sig];
                for sig in &child_sigs {
                    // Use only the first line of each child signature too.
                    let first_line = sig.lines().next().unwrap_or(sig);
                    parts.push(format!("    {first_line}"));
                }
                parts.join("\n")
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
            continue;
        }

        let source = if let Some(end) = end_line {
            let suppress = options.suppress;
            let content = file_cache.entry(file.clone()).or_insert_with(|| {
                let abs_path = repo_root.join(&file);
                match abs_path.canonicalize() {
                    Ok(canonical) if canonical.starts_with(&canonical_root) => {
                        std::fs::read_to_string(&canonical).ok()
                    }
                    Ok(_) => {
                        output::print_hint(
                            &format!("path outside repo root, skipping: {file}"),
                            suppress,
                        );
                        None
                    }
                    Err(_) => {
                        output::print_hint(&format!("source file not found: {file}"), suppress);
                        None
                    }
                }
            });
            match content {
                Some(c) => extract_lines(c, line, end),
                None => continue,
            }
        } else {
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

/// Query child symbol signatures using a pre-prepared statement.
///
/// Returns signatures of symbols whose `scope` matches the parent name and
/// that reside in the same file, ordered by line number.
fn query_child_signatures_with_stmt(
    stmt: &mut rusqlite::Statement<'_>,
    parent_name: &str,
    file: &str,
) -> Result<Vec<String>> {
    let sigs = stmt
        .query_map(rusqlite::params![parent_name, file], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(sigs)
}

/// Escape SQLite LIKE wildcards (`%`, `_`, `\`) in user input.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Extract lines `start..=end` (1-based) from content.
fn extract_lines(content: &str, start: usize, end: usize) -> String {
    let count = end.saturating_sub(start) + 1;
    content
        .lines()
        .skip(start.saturating_sub(1))
        .take(count)
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
            suppress: true,
            shallow: false,
            scope: None,
            signatures_only: false,
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
    fn no_end_line_fallback_to_signature_via_db() {
        // Insert a symbol directly with end_line = NULL to reliably test
        // the signature fallback path, independent of tree-sitter grammar.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();

        pipeline::build_index(root, true).unwrap();
        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        // Insert a synthetic symbol with no end_line.
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, end_line, scope, signature, language) \
             VALUES ('MY_CONST', 'constant', 'src/lib.rs', 3, 0, NULL, NULL, 'const MY_CONST: usize = 1024', 'Rust')",
            [],
        )
        .unwrap();

        let results = show_symbol(&conn, "MY_CONST", root, &default_options()).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, "const MY_CONST: usize = 1024");
        assert!(results[0].end_line.is_none());
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

    #[test]
    fn escape_like_wildcards() {
        assert_eq!(escape_like("hello"), "hello");
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("foo_bar"), "foo\\_bar");
        assert_eq!(escape_like("a\\b"), "a\\\\b");
        assert_eq!(escape_like("%_\\"), "\\%\\_\\\\");
    }

    #[test]
    fn shallow_struct_shows_child_signatures() {
        let source = "struct Foo {\n    x: i32,\n}\n\nimpl Foo {\n    fn bar(&self) -> i32 {\n        self.x\n    }\n    fn baz(&self) -> bool {\n        true\n    }\n}\n";
        let (dir, conn) = make_indexed_repo(source);

        let opts = ShowOptions {
            shallow: true,
            exact: true,
            kind: Some("struct".into()),
            ..default_options()
        };
        let results = show_symbol(&conn, "Foo", dir.path(), &opts).unwrap();

        // Should find the struct Foo
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.name, "Foo");
        // Source should contain the struct signature and child method signatures
        // but NOT the method bodies
        assert!(r.source.contains("Foo"), "should contain struct name");
        assert!(r.source.contains("bar"), "should contain child method bar");
        assert!(r.source.contains("baz"), "should contain child method baz");
        assert!(
            !r.source.contains("self.x"),
            "should NOT contain method body"
        );
        assert!(!r.source.contains("true"), "should NOT contain method body");
    }

    #[test]
    fn shallow_non_container_shows_full_body() {
        let source = "fn hello() {\n    println!(\"hi\");\n}\n";
        let (dir, conn) = make_indexed_repo(source);

        let opts = ShowOptions {
            shallow: true,
            ..default_options()
        };
        let results = show_symbol(&conn, "hello", dir.path(), &opts).unwrap();

        assert_eq!(results.len(), 1);
        // Non-container with shallow: falls through to full body
        assert!(results[0].source.contains("println!"));
    }

    #[test]
    fn shallow_no_children_shows_signature_only() {
        // Insert a synthetic struct with no children via direct DB insert
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "struct Empty;\n").unwrap();

        pipeline::build_index(root, true).unwrap();
        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        // Override: insert a container with no children to test edge case
        conn.execute("DELETE FROM symbols WHERE name = 'Empty'", [])
            .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, end_line, scope, signature, language) \
             VALUES ('EmptyClass', 'class', 'src/lib.rs', 1, 0, 5, NULL, 'class EmptyClass', 'TypeScript')",
            [],
        ).unwrap();

        let opts = ShowOptions {
            shallow: true,
            exact: true,
            ..default_options()
        };
        let results = show_symbol(&conn, "EmptyClass", root, &opts).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, "class EmptyClass");
    }

    #[test]
    fn multiple_symbols_same_file_cached() {
        // Two functions in the same file: file should only be read once.
        let source = "fn alpha() {\n    1\n}\nfn beta() {\n    2\n}\n";
        let (dir, conn) = make_indexed_repo(source);

        let results = show_symbol(&conn, "", dir.path(), &default_options()).unwrap();

        // Both functions should be found.
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn show_file_returns_top_level_symbols() {
        let source = "fn alpha() {\n    1\n}\nfn beta() {\n    2\n}\n";
        let (dir, conn) = make_indexed_repo(source);

        let results = show_file(&conn, "src/lib.rs", dir.path(), &default_options()).unwrap();

        assert_eq!(results.len(), 2);
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn show_file_directory_prefix() {
        let source = "fn alpha() {\n    1\n}\n";
        let (dir, conn) = make_indexed_repo(source);

        let results = show_file(&conn, "src/", dir.path(), &default_options()).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "alpha");
    }

    #[test]
    fn show_file_excludes_scoped_symbols() {
        // Methods (scope != NULL) should be excluded from file-only mode.
        let source = "struct Foo {}\nimpl Foo {\n    fn bar(&self) {\n        42\n    }\n}\n";
        let (dir, conn) = make_indexed_repo(source);

        let results = show_file(&conn, "src/lib.rs", dir.path(), &default_options()).unwrap();

        // Only the struct should appear, not the method.
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"Foo"), "should contain top-level Foo");
        assert!(!names.contains(&"bar"), "should exclude scoped method bar");
    }

    #[test]
    fn show_file_respects_shallow_mode() {
        let source = "struct Foo {\n    x: i32,\n}\n\nimpl Foo {\n    fn bar(&self) -> i32 {\n        self.x\n    }\n}\n";
        let (dir, conn) = make_indexed_repo(source);

        let opts = ShowOptions {
            shallow: true,
            ..default_options()
        };
        let results = show_file(&conn, "src/lib.rs", dir.path(), &opts).unwrap();

        let foo = results
            .iter()
            .find(|r| r.name == "Foo")
            .expect("should find Foo");
        assert!(
            foo.source.contains("bar"),
            "shallow should include child signatures"
        );
        assert!(
            !foo.source.contains("self.x"),
            "shallow should not include method bodies"
        );
    }

    #[test]
    fn show_file_no_match_returns_empty() {
        let source = "fn alpha() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        let results = show_file(&conn, "nonexistent.rs", dir.path(), &default_options()).unwrap();
        assert!(results.is_empty());
    }
}
