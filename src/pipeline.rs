//! Full index build pipeline.
//!
//! Orchestrates `wonk init` and `wonk update` by combining:
//! - File walking ([`crate::walker`])
//! - Tree-sitter parsing and extraction ([`crate::indexer`])
//! - SQLite storage ([`crate::db`])
//! - Content hashing (xxhash)
//! - Parallel file processing (rayon)
//!
//! Also provides incremental re-indexing functions for use by the daemon
//! file watcher: [`reindex_file`], [`remove_file`], [`index_new_file`],
//! and [`process_events`].

use std::collections::HashSet;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::Connection;

use crate::db;
use crate::indexer;
use crate::progress::Progress;
use crate::types::{Reference, Symbol};
use crate::walker::Walker;
use crate::watcher::FileEvent;

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
    /// Extracted import paths for dependency graph.
    imports: Vec<String>,
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
    build_index_with_progress(repo_root, local, &Progress::silent())
}

/// Build a fresh index with progress reporting.
///
/// Same as [`build_index`] but calls `progress.set_total()` after the walker
/// pre-scan and `progress.inc()` after each file is parsed.
pub fn build_index_with_progress(
    repo_root: &Path,
    local: bool,
    progress: &Progress,
) -> Result<IndexStats> {
    let start = Instant::now();

    // 1. Determine index path.
    let index_path = db::index_path_for(repo_root, local)?;

    // 2. Open (or create) the database.
    let conn = db::open(&index_path)?;

    // 3. Walk files.
    let paths = Walker::new(repo_root).collect_paths();

    // Set total for progress reporting.
    progress.set_total(paths.len());

    // 4. Parse in parallel.
    let results: Vec<FileResult> = paths
        .par_iter()
        .filter_map(|path| {
            let result = parse_one_file(path, repo_root);
            progress.inc();
            result
        })
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
    rebuild_index_with_progress(repo_root, local, &Progress::silent())
}

/// Drop all data and rebuild the index with progress reporting.
///
/// Same as [`rebuild_index`] but forwards `progress` to
/// [`build_index_with_progress`].
pub fn rebuild_index_with_progress(
    repo_root: &Path,
    local: bool,
    progress: &Progress,
) -> Result<IndexStats> {
    let index_path = db::index_path_for(repo_root, local)?;

    // If the database exists, drop all data.
    if index_path.exists() {
        let conn = db::open(&index_path)?;
        drop_all_data(&conn)?;
        drop(conn);
    }

    build_index_with_progress(repo_root, local, progress)
}

// ---------------------------------------------------------------------------
// Incremental re-indexing API
// ---------------------------------------------------------------------------

/// Re-index a single file if its content has changed.
///
/// Computes the xxhash of the file's current content and compares it to the
/// stored hash in the `files` table.  If the hash is unchanged the file is
/// skipped and this function returns `Ok(false)`.
///
/// When the hash differs (or the file is not yet in the index), the old
/// symbols and references for that file are deleted and the file is re-parsed
/// and re-inserted in a single transaction.  Returns `Ok(true)` when the
/// file was actually re-indexed.
pub fn reindex_file(conn: &Connection, file_path: &Path, repo_root: &Path) -> Result<bool> {
    // Compute the relative path used as the key in the DB.
    let rel_path = file_path
        .strip_prefix(repo_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .into_owned();

    // Read the current content.
    let content = std::fs::read_to_string(file_path)
        .with_context(|| format!("reading file {}", file_path.display()))?;

    // Compute content hash.
    let new_hash = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(content.as_bytes()));

    // Compare with stored hash — skip if unchanged.
    let stored_hash: Option<String> = conn
        .query_row(
            "SELECT hash FROM files WHERE path = ?1",
            rusqlite::params![rel_path],
            |row| row.get(0),
        )
        .ok();

    if stored_hash.as_deref() == Some(new_hash.as_str()) {
        return Ok(false);
    }

    // Detect language — if unsupported, remove stale data and return.
    let lang = match indexer::detect_language(file_path) {
        Some(l) => l,
        None => {
            // File is not a supported language.  If it was previously indexed
            // (unlikely), clean it up.
            delete_file_data(conn, &rel_path)?;
            return Ok(false);
        }
    };

    // Parse with tree-sitter.
    let mut parser = indexer::get_parser(lang);
    let tree = parser
        .parse(content.as_bytes(), None)
        .context("tree-sitter parse failed")?;

    let symbols = indexer::extract_symbols(&tree, &content, &rel_path, lang);
    let refs = indexer::extract_references(&tree, &content, &rel_path, lang);
    let file_imports = indexer::extract_imports(&tree, &content, &rel_path, lang);
    let line_count = content.lines().count();

    // Single transaction: delete old data, insert new data.
    upsert_file_data(conn, &FileResult {
        rel_path,
        language: lang.name().to_string(),
        content_hash: new_hash,
        line_count,
        symbols,
        refs,
        imports: file_imports.imports,
    })?;

    Ok(true)
}

/// Remove all indexed data for a deleted file.
///
/// Deletes the file's row from `files`, all its symbols from `symbols`,
/// and all its references from `"references"`.  The FTS5 content-sync
/// triggers handle updating `symbols_fts` automatically.
pub fn remove_file(conn: &Connection, file_path: &Path, repo_root: &Path) -> Result<()> {
    let rel_path = file_path
        .strip_prefix(repo_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .into_owned();

    delete_file_data(conn, &rel_path)
}

/// Index a newly created file.
///
/// Detects the language, parses the file with tree-sitter, and inserts the
/// file metadata, symbols, and references into the database.  If the file
/// has an unsupported language extension, this is a no-op.
pub fn index_new_file(conn: &Connection, file_path: &Path, repo_root: &Path) -> Result<()> {
    // Delegate to reindex_file — it handles the "not yet in index" case
    // identically to "hash changed" (the stored hash will be None, so the
    // comparison will always trigger a full index).
    let _ = reindex_file(conn, file_path, repo_root)?;
    Ok(())
}

/// Process a batch of file change events, returning the number of files
/// that were actually updated (re-indexed or removed).
///
/// Events are processed sequentially.  Errors on individual files are
/// logged (via the returned Result) but do not abort the entire batch;
/// processing continues with the remaining events.
pub fn process_events(
    conn: &Connection,
    events: &[FileEvent],
    repo_root: &Path,
) -> Result<usize> {
    let mut updated = 0usize;

    for event in events {
        let result = match event {
            FileEvent::Created(path) => {
                index_new_file(conn, path, repo_root).map(|()| true)
            }
            FileEvent::Modified(path) => {
                reindex_file(conn, path, repo_root)
            }
            FileEvent::Deleted(path) => {
                remove_file(conn, path, repo_root).map(|()| true)
            }
        };

        match result {
            Ok(true) => updated += 1,
            Ok(false) => {} // unchanged
            Err(e) => {
                // Log the error but continue processing the batch.
                eprintln!(
                    "warn: failed to process {}: {:#}",
                    event.path().display(),
                    e
                );
            }
        }
    }

    Ok(updated)
}

// ---------------------------------------------------------------------------
// Internals — incremental helpers
// ---------------------------------------------------------------------------

/// Delete all data for a single file (symbols, references, file row) in a
/// single transaction.
fn delete_file_data(conn: &Connection, rel_path: &str) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .context("starting delete transaction")?;

    tx.execute(
        "DELETE FROM symbols WHERE file = ?1",
        rusqlite::params![rel_path],
    )?;
    tx.execute(
        "DELETE FROM \"references\" WHERE file = ?1",
        rusqlite::params![rel_path],
    )?;
    tx.execute(
        "DELETE FROM file_imports WHERE source_file = ?1",
        rusqlite::params![rel_path],
    )?;
    tx.execute(
        "DELETE FROM files WHERE path = ?1",
        rusqlite::params![rel_path],
    )?;

    tx.commit().context("committing delete transaction")?;
    Ok(())
}

/// Delete old data for a file and insert the new parse results in a single
/// transaction.
fn upsert_file_data(conn: &Connection, result: &FileResult) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let tx = conn
        .unchecked_transaction()
        .context("starting upsert transaction")?;

    // Delete old symbols, references, and imports for this file.
    tx.execute(
        "DELETE FROM symbols WHERE file = ?1",
        rusqlite::params![result.rel_path],
    )?;
    tx.execute(
        "DELETE FROM \"references\" WHERE file = ?1",
        rusqlite::params![result.rel_path],
    )?;
    tx.execute(
        "DELETE FROM file_imports WHERE source_file = ?1",
        rusqlite::params![result.rel_path],
    )?;

    // Upsert file metadata.
    tx.execute(
        "INSERT OR REPLACE INTO files (path, language, hash, last_indexed, line_count, symbols_count) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            result.rel_path,
            result.language,
            result.content_hash,
            now,
            result.line_count as i64,
            result.symbols.len() as i64,
        ],
    )?;

    // Insert new symbols.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO symbols (name, kind, file, line, col, end_line, scope, signature, language) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for sym in &result.symbols {
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
        }
    }

    // Insert new references.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO \"references\" (name, file, line, col, context) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for reference in &result.refs {
            stmt.execute(rusqlite::params![
                reference.name,
                reference.file,
                reference.line as i64,
                reference.col as i64,
                reference.context,
            ])?;
        }
    }

    // Insert new imports.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
        )?;
        for import in &result.imports {
            stmt.execute(rusqlite::params![result.rel_path, import])?;
        }
    }

    tx.commit().context("committing upsert transaction")?;
    Ok(())
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

    // Extract imports for dependency graph.
    let file_imports = indexer::extract_imports(&tree, &content, &rel_path, lang);

    let line_count = content.lines().count();

    Some(FileResult {
        rel_path,
        language: lang.name().to_string(),
        content_hash: hash,
        line_count,
        symbols,
        refs,
        imports: file_imports.imports,
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

    // Insert file imports.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
        )?;
        for r in results {
            for import in &r.imports {
                stmt.execute(rusqlite::params![r.rel_path, import])?;
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
         DELETE FROM file_imports;
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
    fn test_build_index_stores_imports() {
        let dir = make_test_repo();
        let _stats = build_index(dir.path(), true).unwrap();

        let index_path = db::local_index_path(dir.path());
        let conn = db::open_existing(&index_path).unwrap();

        // The Rust file has `use std::io;` so we should find at least one import.
        let import_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_imports WHERE source_file = 'src/main.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            import_count > 0,
            "should store imports from src/main.rs, got {import_count}"
        );

        // Python file has `import os` so should have imports too.
        let py_imports: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_imports WHERE source_file = 'app.py'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            py_imports > 0,
            "should store imports from app.py, got {py_imports}"
        );
    }

    #[test]
    fn test_reindex_file_updates_imports() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Initially lib.rs has no imports.
        let orig_imports: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_imports WHERE source_file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orig_imports, 0, "lib.rs should have no imports initially");

        // Rewrite lib.rs to include an import.
        fs::write(root.join("lib.rs"), "use std::io;\nfn hello() { 1 }").unwrap();
        reindex_file(&conn, &root.join("lib.rs"), root).unwrap();

        let new_imports: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_imports WHERE source_file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            new_imports > 0,
            "lib.rs should have imports after rewrite, got {new_imports}"
        );
    }

    #[test]
    fn test_remove_file_deletes_imports() {
        let dir = make_test_repo();
        let _stats = build_index(dir.path(), true).unwrap();

        let index_path = db::local_index_path(dir.path());
        let conn = db::open_existing(&index_path).unwrap();

        // Verify imports exist before removal.
        let before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_imports WHERE source_file = 'src/main.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(before > 0, "should have imports before removal");

        remove_file(&conn, &dir.path().join("src/main.rs"), dir.path()).unwrap();

        let after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_imports WHERE source_file = 'src/main.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(after, 0, "imports should be removed after file removal");
    }

    #[test]
    fn test_rebuild_clears_imports() {
        let dir = make_test_repo();
        let _stats1 = build_index(dir.path(), true).unwrap();

        let index_path = db::local_index_path(dir.path());
        let conn1 = db::open_existing(&index_path).unwrap();
        let count1: i64 = conn1
            .query_row("SELECT COUNT(*) FROM file_imports", [], |row| row.get(0))
            .unwrap();
        assert!(count1 > 0);
        drop(conn1);

        // Rebuild should not double the imports.
        let _stats2 = rebuild_index(dir.path(), true).unwrap();
        let conn2 = db::open_existing(&index_path).unwrap();
        let count2: i64 = conn2
            .query_row("SELECT COUNT(*) FROM file_imports", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count1, count2, "rebuild should not duplicate imports");
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

    // -----------------------------------------------------------------------
    // Incremental re-indexing tests
    // -----------------------------------------------------------------------

    /// Helper: create a repo, build initial index, return (dir, conn).
    fn setup_indexed_repo() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join("lib.rs"), "fn hello() { 1 }\nfn world() { 2 }").unwrap();
        fs::write(root.join("app.py"), "def greet():\n    pass\n").unwrap();

        let _stats = build_index(root, true).unwrap();
        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        (dir, conn)
    }

    #[test]
    fn test_reindex_file_unchanged_skips() {
        let (dir, conn) = setup_indexed_repo();
        // File content hasn't changed — reindex_file should return false.
        let changed = reindex_file(&conn, &dir.path().join("lib.rs"), dir.path()).unwrap();
        assert!(!changed, "unchanged file should be skipped");
    }

    #[test]
    fn test_reindex_file_changed_updates() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Record the original hash and symbol count.
        let orig_hash: String = conn
            .query_row("SELECT hash FROM files WHERE path = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        let orig_sym_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        assert!(orig_sym_count > 0, "should have symbols initially");

        // Modify the file to have different content (add a new function).
        fs::write(
            root.join("lib.rs"),
            "fn hello() { 1 }\nfn world() { 2 }\nfn added() { 3 }",
        )
        .unwrap();

        let changed = reindex_file(&conn, &root.join("lib.rs"), root).unwrap();
        assert!(changed, "modified file should be re-indexed");

        // Hash should have changed.
        let new_hash: String = conn
            .query_row("SELECT hash FROM files WHERE path = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        assert_ne!(orig_hash, new_hash, "hash should change after modification");

        // Symbol count should have increased (we added a function).
        let new_sym_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        assert!(
            new_sym_count > orig_sym_count,
            "should have more symbols after adding a function: {new_sym_count} vs {orig_sym_count}"
        );
    }

    #[test]
    fn test_reindex_file_updates_metadata() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        let orig_indexed: i64 = conn
            .query_row("SELECT last_indexed FROM files WHERE path = 'lib.rs'", [], |row| row.get(0))
            .unwrap();

        // Change the file.
        fs::write(root.join("lib.rs"), "fn only_one() {}").unwrap();
        let changed = reindex_file(&conn, &root.join("lib.rs"), root).unwrap();
        assert!(changed);

        // last_indexed should be updated.
        let new_indexed: i64 = conn
            .query_row("SELECT last_indexed FROM files WHERE path = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        assert!(new_indexed >= orig_indexed, "last_indexed should be updated");

        // symbols_count should reflect the new file content.
        let sym_count_meta: i64 = conn
            .query_row("SELECT symbols_count FROM files WHERE path = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        let sym_count_actual: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sym_count_meta, sym_count_actual, "symbols_count metadata should match actual count");
    }

    #[test]
    fn test_reindex_file_replaces_old_symbols() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Initially we have 'hello' and 'world' functions.
        let has_hello: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs' AND name = 'hello'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            > 0;
        assert!(has_hello, "should have 'hello' symbol initially");

        // Rewrite the file with completely different symbols.
        fs::write(root.join("lib.rs"), "fn alpha() {}\nfn beta() {}").unwrap();
        reindex_file(&conn, &root.join("lib.rs"), root).unwrap();

        // Old symbols should be gone.
        let has_hello_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs' AND name = 'hello'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_hello_after, 0, "'hello' symbol should be removed after re-index");

        // New symbols should be present.
        let has_alpha: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs' AND name = 'alpha'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_alpha > 0, "'alpha' symbol should be present after re-index");
    }

    #[test]
    fn test_reindex_file_updates_fts() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Verify initial FTS state.
        let fts_hello: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'hello'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fts_hello > 0, "FTS should contain 'hello' initially");

        // Rewrite file without 'hello'.
        fs::write(root.join("lib.rs"), "fn replacement() {}").unwrap();
        reindex_file(&conn, &root.join("lib.rs"), root).unwrap();

        // 'hello' should be gone from FTS.
        let fts_hello_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'hello'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fts_hello_after, 0, "FTS should not contain 'hello' after re-index");

        // 'replacement' should be in FTS.
        let fts_replacement: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'replacement'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fts_replacement > 0, "FTS should contain 'replacement' after re-index");
    }

    #[test]
    fn test_remove_file_deletes_all_data() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Verify data exists before removal.
        let file_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files WHERE path = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(file_count, 1);

        let sym_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        assert!(sym_count > 0);

        // Remove the file from the index.
        remove_file(&conn, &root.join("lib.rs"), root).unwrap();

        // All data should be gone.
        let file_count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM files WHERE path = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(file_count_after, 0, "files row should be removed");

        let sym_count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sym_count_after, 0, "symbols should be removed");

        let ref_count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM \"references\" WHERE file = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ref_count_after, 0, "references should be removed");
    }

    #[test]
    fn test_remove_file_updates_fts() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Verify FTS has data.
        let fts_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'hello'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fts_before > 0);

        remove_file(&conn, &root.join("lib.rs"), root).unwrap();

        let fts_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'hello'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fts_after, 0, "FTS should be cleaned up after file removal");
    }

    #[test]
    fn test_remove_file_nonexistent_is_ok() {
        let (dir, conn) = setup_indexed_repo();
        // Removing a file that doesn't exist in the index should not error.
        remove_file(&conn, &dir.path().join("nonexistent.rs"), dir.path()).unwrap();
    }

    #[test]
    fn test_remove_file_leaves_other_files_intact() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Remove lib.rs but app.py should remain.
        remove_file(&conn, &root.join("lib.rs"), root).unwrap();

        let py_file: i64 = conn
            .query_row("SELECT COUNT(*) FROM files WHERE path = 'app.py'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(py_file, 1, "app.py should still be in the index");

        let py_syms: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols WHERE file = 'app.py'", [], |row| row.get(0))
            .unwrap();
        assert!(py_syms > 0, "app.py symbols should still be in the index");
    }

    #[test]
    fn test_index_new_file() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Create a new file not yet in the index.
        fs::write(root.join("new_file.rs"), "fn brand_new() {}\nstruct Fresh {}").unwrap();

        index_new_file(&conn, &root.join("new_file.rs"), root).unwrap();

        // File should be in the index.
        let file_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'new_file.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(file_count, 1, "new file should be in files table");

        // Symbols should be extracted.
        let sym_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'new_file.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sym_count > 0, "new file should have symbols");

        // FTS should be updated.
        let fts_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'brand_new'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fts_count > 0, "FTS should contain symbols from new file");
    }

    #[test]
    fn test_index_new_file_unsupported_extension() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        fs::write(root.join("readme.txt"), "not code").unwrap();
        // Should not error, just a no-op.
        index_new_file(&conn, &root.join("readme.txt"), root).unwrap();

        let file_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'readme.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(file_count, 0, "unsupported file should not be indexed");
    }

    #[test]
    fn test_process_events_mixed_batch() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Modify an existing file.
        fs::write(root.join("lib.rs"), "fn modified_func() {}").unwrap();

        // Create a new file.
        fs::write(root.join("extra.rs"), "fn extra() {}").unwrap();

        // "Delete" app.py (just remove from index; the file still exists on
        // disk but Deleted event means the watcher says it's gone).
        let events = vec![
            FileEvent::Modified(root.join("lib.rs")),
            FileEvent::Created(root.join("extra.rs")),
            FileEvent::Deleted(root.join("app.py")),
        ];

        let updated = process_events(&conn, &events, root).unwrap();
        // All three should count as updates (modify changed hash, new file, delete).
        assert_eq!(updated, 3, "all three events should result in updates");

        // lib.rs should have the new symbol.
        let has_modified: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs' AND name = 'modified_func'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_modified > 0, "modified file should have new symbols");

        // extra.rs should be indexed.
        let has_extra: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'extra.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_extra, 1, "new file should be indexed");

        // app.py should be removed.
        let has_py: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'app.py'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_py, 0, "deleted file should be removed");
    }

    #[test]
    fn test_process_events_empty_batch() {
        let (dir, conn) = setup_indexed_repo();
        let events: Vec<FileEvent> = vec![];
        let updated = process_events(&conn, &events, dir.path()).unwrap();
        assert_eq!(updated, 0, "empty batch should produce 0 updates");
    }

    #[test]
    fn test_process_events_unchanged_file() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Send a Modified event for a file that hasn't actually changed.
        let events = vec![FileEvent::Modified(root.join("lib.rs"))];
        let updated = process_events(&conn, &events, root).unwrap();
        assert_eq!(updated, 0, "unchanged file should not count as updated");
    }

    #[test]
    fn test_process_events_continues_on_error() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // First event: a file that doesn't exist (will fail to read).
        // Second event: a valid modification.
        fs::write(root.join("lib.rs"), "fn changed_after_error() {}").unwrap();

        let events = vec![
            FileEvent::Modified(root.join("ghost.rs")),
            FileEvent::Modified(root.join("lib.rs")),
        ];

        let updated = process_events(&conn, &events, root).unwrap();
        // The ghost.rs error should not prevent lib.rs from being processed.
        assert_eq!(updated, 1, "should process remaining events after error");

        let has_changed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs' AND name = 'changed_after_error'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_changed > 0, "lib.rs should be re-indexed despite earlier error");
    }

    #[test]
    fn test_build_index_with_progress_sets_total_and_done() {
        use crate::progress::{Progress, ProgressMode};
        use std::sync::Arc;

        let dir = make_test_repo();
        let progress = Arc::new(Progress::new("Indexing", "Indexed", ProgressMode::Silent));

        let stats = build_index_with_progress(dir.path(), true, &progress).unwrap();

        // Progress total should equal the number of walker paths (>= file_count since
        // some paths may be unsupported languages).
        assert!(progress.total() > 0, "progress total should be set");
        // Done should equal total (all files processed).
        assert_eq!(progress.done(), progress.total(), "all files should be processed");
        // Stats should still be correct.
        assert!(stats.file_count >= 3);
        assert!(stats.symbol_count > 0);
    }

    #[test]
    fn test_rebuild_index_with_progress() {
        use crate::progress::{Progress, ProgressMode};
        use std::sync::Arc;

        let dir = make_test_repo();
        // Build first.
        let _stats1 = build_index(dir.path(), true).unwrap();

        let progress = Arc::new(Progress::new("Re-indexing", "Re-indexed", ProgressMode::Silent));
        let stats2 = rebuild_index_with_progress(dir.path(), true, &progress).unwrap();

        assert!(progress.total() > 0, "progress total should be set for rebuild");
        assert_eq!(progress.done(), progress.total(), "all files processed in rebuild");
        assert!(stats2.symbol_count > 0);
    }

    #[test]
    fn test_build_index_delegates_to_with_progress() {
        // Ensure the non-progress build_index still works (it delegates internally)
        let dir = make_test_repo();
        let stats = build_index(dir.path(), true).unwrap();
        assert!(stats.file_count >= 3);
        assert!(stats.symbol_count > 0);
    }

    #[test]
    fn test_reindex_file_no_double_symbols() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        let orig_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'", [], |row| row.get(0))
            .unwrap();

        // Modify the file slightly (same symbols, different content to change hash).
        fs::write(root.join("lib.rs"), "fn hello() { 1 }\nfn world() { 2 }\n// comment").unwrap();
        reindex_file(&conn, &root.join("lib.rs"), root).unwrap();

        let new_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'", [], |row| row.get(0))
            .unwrap();
        // Should be the same number (old symbols deleted, new ones inserted).
        assert_eq!(
            orig_count, new_count,
            "symbol count should not double after re-index: orig={orig_count}, new={new_count}"
        );
    }
}
