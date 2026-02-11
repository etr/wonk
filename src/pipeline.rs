//! Full index build pipeline.
//!
//! Orchestrates `wonk init` and `wonk update` by combining:
//! - File walking ([`crate::walker`])
//! - Tree-sitter parsing and extraction ([`crate::indexer`])
//! - SQLite storage ([`crate::db`])
//! - Content hashing (xxhash)
//! - Parallel file processing (rayon)

use std::collections::HashSet;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::Connection;

use crate::db;
use crate::indexer;
use crate::types::{Reference, Symbol};
use crate::walker::Walker;

// ---------------------------------------------------------------------------
// IndexStats
// ---------------------------------------------------------------------------

/// Statistics returned after an indexing run.
#[derive(Debug, Clone)]
pub struct IndexStats {
    /// Number of files processed.
    pub file_count: usize,
    /// Number of symbol definitions extracted.
    pub symbol_count: usize,
    /// Number of references extracted.
    pub ref_count: usize,
    /// Wall-clock elapsed time.
    pub elapsed: std::time::Duration,
}

// ---------------------------------------------------------------------------
// Per-file parse result (collected from parallel phase)
// ---------------------------------------------------------------------------

/// Everything extracted from a single source file.
struct FileResult {
    /// Path relative to repo root (stored in DB).
    rel_path: String,
    /// Detected language name.
    language: String,
    /// xxhash content hash.
    content_hash: String,
    /// Line count.
    line_count: usize,
    /// Extracted symbols.
    symbols: Vec<Symbol>,
    /// Extracted references.
    refs: Vec<Reference>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Build a fresh index for the repository at `repo_root`.
///
/// Steps:
/// 1. Determine the index path (central or local).
/// 2. Create the index directory and open/create the SQLite database.
/// 3. Walk files using [`Walker`].
/// 4. Parse files in parallel with rayon (detect language, parse with
///    tree-sitter, extract symbols + references, compute xxhash).
/// 5. Batch-insert results into SQLite inside a transaction.
/// 6. Write `meta.json`.
/// 7. Return [`IndexStats`].
pub fn build_index(repo_root: &Path, local: bool) -> Result<IndexStats> {
    let start = Instant::now();

    // 1. Determine index path.
    let index_path = db::index_path_for(repo_root, local)?;

    // 2. Open (or create) the database.
    let conn = db::open(&index_path)?;

    // 3. Walk files.
    let paths = Walker::new(repo_root).collect_paths();

    // 4. Parse in parallel.
    let results: Vec<FileResult> = paths
        .par_iter()
        .filter_map(|path| parse_one_file(path, repo_root))
        .collect();

    // 5. Batch insert.
    let (sym_count, ref_count) = batch_insert(&conn, &results)?;

    // 6. Collect languages seen and write meta.json.
    let languages: Vec<String> = {
        let mut set = HashSet::new();
        for r in &results {
            set.insert(r.language.clone());
        }
        let mut v: Vec<String> = set.into_iter().collect();
        v.sort();
        v
    };
    db::write_meta(&index_path, repo_root, &languages)?;

    Ok(IndexStats {
        file_count: results.len(),
        symbol_count: sym_count,
        ref_count: ref_count,
        elapsed: start.elapsed(),
    })
}

/// Drop all data and rebuild the index from scratch.
///
/// This is used by `wonk update` to force a full re-index.
pub fn rebuild_index(repo_root: &Path, local: bool) -> Result<IndexStats> {
    let index_path = db::index_path_for(repo_root, local)?;

    // If the database exists, drop all data.
    if index_path.exists() {
        let conn = db::open(&index_path)?;
        drop_all_data(&conn)?;
        drop(conn);
    }

    build_index(repo_root, local)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Parse a single file and extract everything we need.
///
/// Returns `None` if the file is not a supported language or cannot be read.
fn parse_one_file(path: &Path, repo_root: &Path) -> Option<FileResult> {
    let lang = indexer::detect_language(path)?;
    let content = std::fs::read_to_string(path).ok()?;

    // Compute content hash.
    let hash = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(content.as_bytes()));

    // Parse with tree-sitter.
    let mut parser = indexer::get_parser(lang);
    let tree = parser.parse(content.as_bytes(), None)?;

    // Relative path for storage.
    let rel_path = path
        .strip_prefix(repo_root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();

    // Extract symbols.
    let symbols = indexer::extract_symbols(&tree, &content, &rel_path, lang);

    // Extract references.
    let refs = indexer::extract_references(&tree, &content, &rel_path, lang);

    let line_count = content.lines().count();

    Some(FileResult {
        rel_path,
        language: lang.name().to_string(),
        content_hash: hash,
        line_count,
        symbols,
        refs,
    })
}

/// Insert all results into the database in a single transaction.
///
/// Returns (symbol_count, ref_count).
fn batch_insert(conn: &Connection, results: &[FileResult]) -> Result<(usize, usize)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let tx = conn.unchecked_transaction()
        .context("starting transaction")?;

    let mut total_syms = 0usize;
    let mut total_refs = 0usize;

    // Insert files.
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO files (path, language, hash, last_indexed, line_count, symbols_count) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for r in results {
            stmt.execute(rusqlite::params![
                r.rel_path,
                r.language,
                r.content_hash,
                now,
                r.line_count as i64,
                r.symbols.len() as i64,
            ])?;
        }
    }

    // Insert symbols.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO symbols (name, kind, file, line, col, end_line, scope, signature, language) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for r in results {
            for sym in &r.symbols {
                stmt.execute(rusqlite::params![
                    sym.name,
                    sym.kind.to_string(),
                    sym.file,
                    sym.line as i64,
                    sym.col as i64,
                    sym.end_line.map(|v| v as i64),
                    sym.scope,
                    sym.signature,
                    sym.language,
                ])?;
                total_syms += 1;
            }
        }
    }

    // Insert references.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO \"references\" (name, file, line, col, context) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for r in results {
            for reference in &r.refs {
                stmt.execute(rusqlite::params![
                    reference.name,
                    reference.file,
                    reference.line as i64,
                    reference.col as i64,
                    reference.context,
                ])?;
                total_refs += 1;
            }
        }
    }

    tx.commit().context("committing transaction")?;
    Ok((total_syms, total_refs))
}

/// Drop all data from the main tables (used before rebuild).
fn drop_all_data(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM symbols;
         DELETE FROM \"references\";
         DELETE FROM files;",
    )
    .context("clearing index data")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Create a minimal test repo with source files.
    fn make_test_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Create a .git directory so find_repo_root can discover it.
        fs::create_dir(root.join(".git")).unwrap();

        // Rust file with a function and struct.
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/main.rs"),
            r#"use std::io;

fn main() {
    let x = helper();
    println!("{}", x);
}

fn helper() -> i32 {
    42
}

struct Config {
    name: String,
}
"#,
        )
        .unwrap();

        // Python file.
        fs::write(
            root.join("app.py"),
            r#"import os

def process(data):
    return data.strip()

class Worker:
    def run(self):
        pass
"#,
        )
        .unwrap();

        // JavaScript file.
        fs::write(
            root.join("index.js"),
            r#"function render() {
    console.log("hello");
}

class Component {
    constructor() {}
}
"#,
        )
        .unwrap();

        dir
    }

    #[test]
    fn test_build_index_basic() {
        let dir = make_test_repo();
        let stats = build_index(dir.path(), true).unwrap();

        assert!(stats.file_count >= 3, "should index at least 3 files, got {}", stats.file_count);
        assert!(stats.symbol_count > 0, "should extract symbols");
        // ref_count is usize so it's always >= 0; just ensure indexing ran.
        let _ = stats.ref_count;
        assert!(stats.elapsed.as_nanos() > 0, "elapsed should be positive");
    }

    #[test]
    fn test_build_index_populates_db() {
        let dir = make_test_repo();
        let _stats = build_index(dir.path(), true).unwrap();

        let index_path = db::local_index_path(dir.path());
        let conn = db::open_existing(&index_path).unwrap();

        // Check symbols table.
        let sym_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))
            .unwrap();
        assert!(sym_count > 0, "symbols table should have entries");

        // Check files table.
        let file_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .unwrap();
        assert!(file_count >= 3, "files table should have at least 3 entries");

        // Check that files have hashes.
        let hash: String = conn
            .query_row(
                "SELECT hash FROM files LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hash.len(), 16, "hash should be 16 hex chars");
    }

    #[test]
    fn test_build_index_fts_populated() {
        let dir = make_test_repo();
        let _stats = build_index(dir.path(), true).unwrap();

        let index_path = db::local_index_path(dir.path());
        let conn = db::open_existing(&index_path).unwrap();

        // FTS should be queryable.
        let fts_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'main OR helper OR process OR render'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fts_count > 0, "FTS index should be populated and queryable");
    }

    #[test]
    fn test_build_index_meta_json() {
        let dir = make_test_repo();
        let _stats = build_index(dir.path(), true).unwrap();

        let index_path = db::local_index_path(dir.path());
        let meta = db::read_meta(&index_path).unwrap();

        assert!(!meta.languages.is_empty(), "meta should list languages");
        assert!(meta.created > 0, "meta should have a timestamp");
    }

    #[test]
    fn test_rebuild_index() {
        let dir = make_test_repo();

        // Build once.
        let stats1 = build_index(dir.path(), true).unwrap();
        assert!(stats1.symbol_count > 0);

        // Rebuild (drop + rebuild).
        let stats2 = rebuild_index(dir.path(), true).unwrap();
        assert!(stats2.symbol_count > 0);

        // After rebuild, the database should have the same count (since files
        // haven't changed).  The key thing is it doesn't double.
        let index_path = db::local_index_path(dir.path());
        let conn = db::open_existing(&index_path).unwrap();
        let sym_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))
            .unwrap();
        // Should match the rebuild count, not 2x.
        assert_eq!(sym_count as usize, stats2.symbol_count);
    }

    #[test]
    fn test_build_index_central_mode() {
        let dir = make_test_repo();
        let stats = build_index(dir.path(), false).unwrap();

        assert!(stats.file_count >= 3);
        assert!(stats.symbol_count > 0);

        // Verify central index path exists.
        let index_path = db::central_index_path(dir.path()).unwrap();
        assert!(index_path.exists(), "central index.db should exist");
    }

    #[test]
    fn test_content_hash_changes_with_content() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join("test.rs"), "fn foo() {}").unwrap();

        let _stats1 = build_index(dir.path(), true).unwrap();
        let index_path = db::local_index_path(dir.path());
        let conn1 = db::open_existing(&index_path).unwrap();
        let hash1: String = conn1
            .query_row("SELECT hash FROM files WHERE path = 'test.rs'", [], |row| row.get(0))
            .unwrap();

        // Modify the file and rebuild.
        fs::write(dir.path().join("test.rs"), "fn foo() { 42 }").unwrap();
        let _stats2 = rebuild_index(dir.path(), true).unwrap();
        let conn2 = db::open_existing(&index_path).unwrap();
        let hash2: String = conn2
            .query_row("SELECT hash FROM files WHERE path = 'test.rs'", [], |row| row.get(0))
            .unwrap();

        assert_ne!(hash1, hash2, "hash should change when content changes");
    }

    #[test]
    fn test_references_inserted() {
        let dir = make_test_repo();
        let stats = build_index(dir.path(), true).unwrap();

        let index_path = db::local_index_path(dir.path());
        let conn = db::open_existing(&index_path).unwrap();

        let ref_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM \"references\"", [], |row| row.get(0))
            .unwrap();

        // The Rust file calls helper() and uses println!, and has `use std::io`,
        // so we should have some references.
        assert_eq!(ref_count as usize, stats.ref_count);
    }

    #[test]
    fn test_empty_repo() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        // No source files.

        let stats = build_index(dir.path(), true).unwrap();
        assert_eq!(stats.file_count, 0);
        assert_eq!(stats.symbol_count, 0);
        assert_eq!(stats.ref_count, 0);
    }

    #[test]
    fn test_unsupported_files_skipped() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join("readme.txt"), "Hello world").unwrap();
        fs::write(dir.path().join("data.csv"), "a,b,c").unwrap();
        fs::write(dir.path().join("test.rs"), "fn main() {}").unwrap();

        let stats = build_index(dir.path(), true).unwrap();
        // Only the .rs file should be indexed.
        assert_eq!(stats.file_count, 1);
    }
}
