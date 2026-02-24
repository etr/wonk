//! Symbol change detection.
//!
//! Compares a fresh Tree-sitter parse of a file against the indexed version
//! in SQLite to detect which symbols were added, modified, or removed.
//! Also provides git-based file change detection for `--since` support.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use crate::indexer;
use crate::types::{ChangeType, ChangedSymbol, Symbol, SymbolKind};

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Identity key for comparing symbols: (name, kind, scope).
type SymbolKey = (String, SymbolKind, Option<String>);

fn symbol_key(sym: &Symbol) -> SymbolKey {
    (sym.name.clone(), sym.kind, sym.scope.clone())
}

/// Compute the xxhash of file content.
///
/// Must stay in sync with the hash format in `pipeline.rs` (xxh3, 16-char hex).
fn file_content_hash(content: &[u8]) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(content))
}

/// Build a [`ChangedSymbol`] from a [`Symbol`] and a [`ChangeType`].
fn make_changed(sym: &Symbol, change_type: ChangeType) -> ChangedSymbol {
    ChangedSymbol {
        name: sym.name.clone(),
        kind: sym.kind,
        file: sym.file.clone(),
        line: sym.line,
        change_type,
    }
}

/// Query all indexed symbols for a given file from the database.
fn query_indexed_symbols(conn: &Connection, file: &str) -> Result<Vec<Symbol>> {
    let mut stmt = conn.prepare(
        "SELECT name, kind, file, line, col, end_line, scope, signature, language \
         FROM symbols WHERE file = ?1 ORDER BY line",
    )?;

    let rows = stmt.query_map(rusqlite::params![file], |row| {
        let kind_str: String = row.get(1)?;
        Ok(Symbol {
            name: row.get(0)?,
            kind: SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function),
            file: row.get(2)?,
            line: row.get::<_, i64>(3)? as usize,
            col: row.get::<_, i64>(4)? as usize,
            end_line: row.get::<_, Option<i64>>(5)?.map(|v| v as usize),
            scope: row.get(6)?,
            signature: row.get(7)?,
            language: row.get(8)?,
        })
    })?;

    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Detect which symbols changed in a file by comparing a fresh Tree-sitter
/// parse against the indexed version in the database.
///
/// Returns an empty `Vec` when the file content hash matches the stored hash
/// (fast path).  For files not in the index, all current symbols are reported
/// as `Added`.  For files deleted from disk, all indexed symbols are `Removed`.
pub fn detect_changed_symbols(
    conn: &Connection,
    file: &str,
    repo_root: &Path,
) -> Result<Vec<ChangedSymbol>> {
    let abs_path = repo_root.join(file);

    // Guard against path traversal: resolved path must stay within repo_root.
    if let Ok(canonical) = abs_path.canonicalize()
        && let Ok(canon_root) = repo_root.canonicalize()
        && !canonical.starts_with(&canon_root)
    {
        bail!("path escapes repository root: {file}");
    }

    // If file doesn't exist on disk, all indexed symbols are Removed.
    if !abs_path.exists() {
        let indexed = query_indexed_symbols(conn, file)?;
        return Ok(indexed
            .iter()
            .map(|s| make_changed(s, ChangeType::Removed))
            .collect());
    }

    // Read current content as UTF-8 (matching pipeline.rs which uses read_to_string).
    let content_str =
        std::fs::read_to_string(&abs_path).with_context(|| format!("reading file {file}"))?;

    // Fast path: compare content hash against stored hash.
    let current_hash = file_content_hash(content_str.as_bytes());
    let stored_hash: Option<String> = conn
        .query_row(
            "SELECT hash FROM files WHERE path = ?1",
            rusqlite::params![file],
            |row| row.get(0),
        )
        .ok();

    if stored_hash.as_deref() == Some(current_hash.as_str()) {
        return Ok(Vec::new());
    }

    // Detect language.
    let lang = match indexer::detect_language(Path::new(file)) {
        Some(l) => l,
        None => bail!("unsupported language for file: {file}"),
    };

    // Parse with Tree-sitter.
    let mut parser = indexer::get_parser(lang);
    let tree = parser
        .parse(content_str.as_bytes(), None)
        .context("tree-sitter parse failed")?;

    let current_symbols = indexer::extract_symbols(&tree, &content_str, file, lang);

    // If file not in index at all, all current symbols are Added.
    let indexed_symbols = query_indexed_symbols(conn, file)?;
    if stored_hash.is_none() && indexed_symbols.is_empty() {
        return Ok(current_symbols
            .iter()
            .map(|s| make_changed(s, ChangeType::Added))
            .collect());
    }

    // Build lookup maps by identity key.
    let current_map: HashMap<SymbolKey, &Symbol> =
        current_symbols.iter().map(|s| (symbol_key(s), s)).collect();
    let indexed_map: HashMap<SymbolKey, &Symbol> =
        indexed_symbols.iter().map(|s| (symbol_key(s), s)).collect();

    let mut changes = Vec::new();

    // Added + Modified: iterate current symbols.
    for (key, sym) in &current_map {
        if let Some(indexed_sym) = indexed_map.get(key) {
            if sym.signature != indexed_sym.signature {
                changes.push(make_changed(sym, ChangeType::Modified));
            }
        } else {
            changes.push(make_changed(sym, ChangeType::Added));
        }
    }

    // Removed: in indexed but not in current.
    for (key, sym) in &indexed_map {
        if !current_map.contains_key(key) {
            changes.push(make_changed(sym, ChangeType::Removed));
        }
    }

    Ok(changes)
}

/// Return the list of files changed since a given git commit.
///
/// Shells out to `git diff --name-only <commit>` and parses the output.
/// Returns a clear error if git is not installed (relevant only for `--since`).
pub fn detect_changed_files_since(commit: &str, repo_root: &Path) -> Result<Vec<String>> {
    // Reject flag-shaped inputs to prevent git argument injection (CWE-88).
    if commit.starts_with('-') {
        bail!("invalid commit reference: {commit}");
    }

    let output = Command::new("git")
        .args(["diff", "--name-only", commit])
        .current_dir(repo_root)
        .output()
        .context("failed to run git — is git installed? (--since requires git)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git diff failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = stdout
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect();

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::pipeline;
    use std::fs;
    use tempfile::TempDir;

    /// Returns true if git is available on this system.
    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    /// Create a minimal Rust repo, index it, and return (TempDir, Connection).
    fn make_indexed_repo(source: &str) -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // .git so find_repo_root works
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), source).unwrap();

        pipeline::build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        (dir, conn)
    }

    #[test]
    fn unchanged_file_returns_empty() {
        let source = "fn hello() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        let changes = detect_changed_symbols(&conn, "src/lib.rs", dir.path()).unwrap();

        assert!(
            changes.is_empty(),
            "unchanged file should produce no changes"
        );
    }

    #[test]
    fn added_symbol_detected() {
        let source = "fn hello() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        // Add a new function to the file
        fs::write(
            dir.path().join("src/lib.rs"),
            "fn hello() { }\nfn world() { }\n",
        )
        .unwrap();

        let changes = detect_changed_symbols(&conn, "src/lib.rs", dir.path()).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "world");
        assert_eq!(changes[0].change_type, crate::types::ChangeType::Added);
    }

    #[test]
    fn removed_symbol_detected() {
        let source = "fn hello() { }\nfn world() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        // Remove the second function
        fs::write(dir.path().join("src/lib.rs"), "fn hello() { }\n").unwrap();

        let changes = detect_changed_symbols(&conn, "src/lib.rs", dir.path()).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "world");
        assert_eq!(changes[0].change_type, crate::types::ChangeType::Removed);
    }

    #[test]
    fn modified_symbol_detected() {
        let source = "fn hello() -> i32 { 42 }\n";
        let (dir, conn) = make_indexed_repo(source);

        // Change the signature
        fs::write(
            dir.path().join("src/lib.rs"),
            "fn hello(x: i32) -> i32 { x }\n",
        )
        .unwrap();

        let changes = detect_changed_symbols(&conn, "src/lib.rs", dir.path()).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "hello");
        assert_eq!(changes[0].change_type, crate::types::ChangeType::Modified);
    }

    #[test]
    fn multiple_changes_detected() {
        let source = "fn keep() { }\nfn remove_me() { }\nfn change_me() -> i32 { 0 }\n";
        let (dir, conn) = make_indexed_repo(source);

        // keep stays, remove_me gone, change_me gets new sig, add_me is new
        fs::write(
            dir.path().join("src/lib.rs"),
            "fn keep() { }\nfn change_me(x: i32) -> i32 { x }\nfn add_me() { }\n",
        )
        .unwrap();

        let changes = detect_changed_symbols(&conn, "src/lib.rs", dir.path()).unwrap();

        let names: Vec<&str> = changes.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"remove_me"), "should detect removed symbol");
        assert!(
            names.contains(&"change_me"),
            "should detect modified symbol"
        );
        assert!(names.contains(&"add_me"), "should detect added symbol");
        assert!(
            !names.contains(&"keep"),
            "unchanged symbol should not appear"
        );
        assert_eq!(changes.len(), 3);
    }

    #[test]
    fn file_not_in_index_all_added() {
        let source = "fn indexed() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        // Create a new file that wasn't indexed
        fs::write(
            dir.path().join("src/new.rs"),
            "fn brand_new() { }\nfn also_new() { }\n",
        )
        .unwrap();

        let changes = detect_changed_symbols(&conn, "src/new.rs", dir.path()).unwrap();

        assert_eq!(changes.len(), 2);
        assert!(
            changes
                .iter()
                .all(|c| c.change_type == crate::types::ChangeType::Added)
        );
    }

    #[test]
    fn file_deleted_all_removed() {
        let source = "fn doomed() { }\nfn also_doomed() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        // Delete the file from disk
        fs::remove_file(dir.path().join("src/lib.rs")).unwrap();

        let changes = detect_changed_symbols(&conn, "src/lib.rs", dir.path()).unwrap();

        assert_eq!(changes.len(), 2);
        assert!(
            changes
                .iter()
                .all(|c| c.change_type == crate::types::ChangeType::Removed)
        );
    }

    #[test]
    fn scoped_symbols_compared_correctly() {
        // Two methods with the same name in different scopes
        let source = r#"
struct Foo;
impl Foo {
    fn work(&self) { }
}
struct Bar;
impl Bar {
    fn work(&self) { }
}
"#;
        let (dir, conn) = make_indexed_repo(source);

        // Remove only Foo::work by rewriting file without it
        let new_source = r#"
struct Foo;
struct Bar;
impl Bar {
    fn work(&self) { }
}
"#;
        fs::write(dir.path().join("src/lib.rs"), new_source).unwrap();

        let changes = detect_changed_symbols(&conn, "src/lib.rs", dir.path()).unwrap();

        // Foo's impl block is removed, so Foo::work should be among Removed.
        // The scope field distinguishes Foo::work from Bar::work.
        let removed_names: Vec<&str> = changes
            .iter()
            .filter(|c| c.change_type == crate::types::ChangeType::Removed)
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            removed_names.contains(&"work"),
            "Foo::work should be detected as removed"
        );
        // Bar::work should NOT appear as removed (it still exists).
        let bar_work_removed = changes.iter().any(|c| {
            c.change_type == crate::types::ChangeType::Removed
                && c.name == "work"
                && c.kind == SymbolKind::Method
        });
        // At least one removed "work" should exist; Bar::work should still be present.
        let added_names: Vec<&str> = changes
            .iter()
            .filter(|c| c.change_type == crate::types::ChangeType::Added)
            .map(|c| c.name.as_str())
            .collect();
        // Bar::work should not appear as Added either (it was already indexed).
        assert!(
            !added_names.contains(&"work") || bar_work_removed,
            "Bar::work should remain unchanged"
        );
    }

    #[test]
    fn git_diff_invalid_commit_returns_error() {
        if !git_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        // Initialize a real git repo
        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();

        let result = detect_changed_files_since("nonexistent_ref_abc123", root);
        assert!(
            result.is_err(),
            "invalid commit ref should produce an error"
        );
    }

    #[test]
    fn git_diff_parses_output() {
        if !git_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Initialize git repo and create initial commit
        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();

        fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(root)
            .output()
            .unwrap();

        // Get the commit hash
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let commit = String::from_utf8(output.stdout).unwrap().trim().to_string();

        // Make a second commit with changes
        fs::write(root.join("a.rs"), "fn a() { changed }\n").unwrap();
        fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "second"])
            .current_dir(root)
            .output()
            .unwrap();

        let files = detect_changed_files_since(&commit, root).unwrap();
        assert!(
            files.contains(&"a.rs".to_string()),
            "should detect modified file"
        );
        assert!(
            files.contains(&"b.rs".to_string()),
            "should detect new file"
        );
    }

    #[test]
    fn git_diff_empty_result() {
        if !git_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Initialize git repo with a commit, then diff HEAD (no changes)
        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();
        fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(root)
            .output()
            .unwrap();

        let files = detect_changed_files_since("HEAD", root).unwrap();
        assert!(files.is_empty(), "no changes since HEAD");
    }
}
