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
        sql.push_str(" AND file LIKE ? ESCAPE '\\'");
        params.push(Box::new(format!("%{}%", escape_like(file_filter))));
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

    // Cache file contents to avoid N+1 I/O when multiple symbols share a file.
    let mut file_cache: HashMap<String, Option<String>> = HashMap::new();
    let canonical_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());

    let mut results = Vec::new();

    for (sym_name, kind_str, file, line, end_line, signature, language) in rows {
        let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
        let line = line as usize;
        let end_line = end_line.map(|v| v as usize);

        // Shallow mode for container types: show container signature + child signatures.
        if options.shallow && kind.is_container() {
            let child_sigs = query_child_signatures(conn, &sym_name, &file)?;
            let source = if child_sigs.is_empty() {
                signature.clone()
            } else {
                let mut parts = vec![signature.clone()];
                for sig in &child_sigs {
                    parts.push(format!("    {sig}"));
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

        // Read source body from disk.
        let source = if let Some(end) = end_line {
            let suppress = options.suppress;
            let content = file_cache.entry(file.clone()).or_insert_with(|| {
                let abs_path = repo_root.join(&file);
                // Validate resolved path stays within repo root (CWE-22).
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

/// Query child symbol signatures for a container (class/struct/enum/trait/interface).
///
/// Returns signatures of symbols whose `scope` matches the parent name and
/// that reside in the same file, ordered by line number.
fn query_child_signatures(conn: &Connection, parent_name: &str, file: &str) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT signature FROM symbols WHERE scope = ?1 AND file = ?2 ORDER BY line")?;
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
}
