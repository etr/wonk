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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::Connection;

use crate::db;
use crate::embedding::{self, OllamaClient};
use crate::errors::EmbeddingError;
use crate::indexer;
use crate::progress::{Progress, ProgressMode};
use crate::types::{RawTypeEdge, Reference, Symbol};
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
    /// Number of references with a resolved caller_id.
    pub caller_count: usize,
    /// Number of type hierarchy edges (extends/implements) stored.
    pub type_edge_count: usize,
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
    /// Extracted type hierarchy edges (extends/implements).
    type_edges: Vec<RawTypeEdge>,
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

    // 2b. Clear any existing data so fresh build is idempotent.
    drop_all_data(&conn)?;

    // 3. Walk files (respecting config ignore patterns).
    let config = crate::config::Config::load(Some(repo_root)).unwrap_or_default();
    let paths = Walker::new(repo_root)
        .with_ignore_patterns(&config.ignore.patterns)
        .collect_paths();

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
    let (sym_count, ref_count, caller_count, type_edge_count) = batch_insert(&conn, &results)?;

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
        ref_count,
        caller_count,
        type_edge_count,
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

/// Incrementally update the index: re-index changed files and remove deleted ones.
///
/// Walks the filesystem, compares with the indexed files table, removes
/// entries for deleted files, and calls [`reindex_file`] for each on-disk
/// file (which skips unchanged files via xxhash comparison).
///
/// Returns [`IndexStats`] reflecting what is now in the database.
pub fn incremental_update(repo_root: &Path, local: bool) -> Result<IndexStats> {
    let start = Instant::now();

    let index_path = db::index_path_for(repo_root, local)?;
    let conn = db::open(&index_path)?;

    // Walk current files on disk.
    let config = crate::config::Config::load(Some(repo_root)).unwrap_or_default();
    let on_disk: HashSet<String> = Walker::new(repo_root)
        .with_ignore_patterns(&config.ignore.patterns)
        .collect_paths()
        .into_iter()
        .filter_map(|p| {
            p.strip_prefix(repo_root)
                .ok()
                .map(|r| r.to_string_lossy().into_owned())
        })
        .collect();

    // Query indexed paths.
    let mut stmt = conn.prepare("SELECT path FROM files")?;
    let indexed: HashSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();

    // Remove files no longer on disk.
    for rel in &indexed {
        if !on_disk.contains(rel) {
            let abs = repo_root.join(rel);
            remove_file(&conn, &abs, repo_root)?;
        }
    }

    // Re-index files on disk (reindex_file skips unchanged via hash).
    for rel in &on_disk {
        let abs = repo_root.join(rel);
        let _ = reindex_file(&conn, &abs, repo_root);
    }

    // Collect languages and rewrite meta.json.
    let mut lang_stmt = conn.prepare("SELECT DISTINCT language FROM files")?;
    let mut languages: Vec<String> = lang_stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    languages.sort();
    db::write_meta(&index_path, repo_root, &languages)?;

    // Gather final stats from DB.
    let file_count = conn
        .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))
        .unwrap_or(0) as usize;
    let symbol_count = conn
        .query_row("SELECT COUNT(*) FROM symbols", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize;
    let ref_count = conn
        .query_row("SELECT COUNT(*) FROM \"references\"", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize;
    let caller_count = conn
        .query_row(
            "SELECT COUNT(*) FROM \"references\" WHERE caller_id IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize;
    let type_edge_count = conn
        .query_row("SELECT COUNT(*) FROM type_edges", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize;

    Ok(IndexStats {
        file_count,
        symbol_count,
        ref_count,
        caller_count,
        type_edge_count,
        elapsed: start.elapsed(),
    })
}

// ---------------------------------------------------------------------------
// ProcessResult
// ---------------------------------------------------------------------------

/// Result of processing a batch of file change events.
#[derive(Debug, Clone)]
pub struct ProcessResult {
    /// Number of files that were actually updated (re-indexed or removed).
    pub updated_count: usize,
    /// Relative paths of the files that changed (created, modified, or deleted).
    pub changed_files: Vec<String>,
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

    // Pre-process Rust source to expand cfg_*! macros so tree-sitter can
    // see the items they wrap.
    let parse_source = if lang == indexer::Lang::Rust {
        indexer::preprocess_rust_macros(&content)
    } else {
        content.clone()
    };

    // Parse with tree-sitter.
    let mut parser = indexer::get_parser(lang);
    let tree = parser
        .parse(parse_source.as_bytes(), None)
        .context("tree-sitter parse failed")?;

    let symbols = indexer::extract_symbols(&tree, &parse_source, &rel_path, lang);
    let mut refs = indexer::extract_references(&tree, &parse_source, &rel_path, lang);
    let file_imports = indexer::extract_imports(&tree, &parse_source, &rel_path, lang);
    let type_edges = indexer::extract_type_edges(&tree, &parse_source, &rel_path, lang);

    // Compute confidence for each reference.
    for r in &mut refs {
        r.confidence = indexer::compute_confidence(r, &symbols, &file_imports.imports);
    }

    let line_count = content.lines().count();

    // Single transaction: delete old data, insert new data.
    upsert_file_data(
        conn,
        &FileResult {
            rel_path,
            language: lang.name().to_string(),
            content_hash: new_hash,
            line_count,
            symbols,
            refs,
            imports: file_imports.imports,
            type_edges,
        },
    )?;

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

/// Process a batch of file change events, returning a [`ProcessResult`]
/// with the count of updated files and their relative paths.
///
/// Events are processed sequentially.  Errors on individual files are
/// logged (via the returned Result) but do not abort the entire batch;
/// processing continues with the remaining events.
pub fn process_events(
    conn: &Connection,
    events: &[FileEvent],
    repo_root: &Path,
) -> Result<ProcessResult> {
    let mut updated = 0usize;
    let mut changed_files = Vec::new();

    for event in events {
        let rel_path = event
            .path()
            .strip_prefix(repo_root)
            .unwrap_or(event.path())
            .to_string_lossy()
            .into_owned();

        let result = match event {
            FileEvent::Created(path) => index_new_file(conn, path, repo_root).map(|()| true),
            FileEvent::Modified(path) => reindex_file(conn, path, repo_root),
            FileEvent::Deleted(path) => remove_file(conn, path, repo_root).map(|()| true),
        };

        match result {
            Ok(true) => {
                updated += 1;
                changed_files.push(rel_path);
            }
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

    Ok(ProcessResult {
        updated_count: updated,
        changed_files,
    })
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

    // Delete type edges before symbols (explicit, mirrors references/imports pattern).
    tx.execute(
        "DELETE FROM type_edges WHERE child_id IN (SELECT id FROM symbols WHERE file = ?1)",
        rusqlite::params![rel_path],
    )?;
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

    // Delete old type edges, symbols, references, and imports for this file.
    // type_edges has ON DELETE CASCADE from symbols, but we delete explicitly
    // for clarity and to mirror the pattern used for references and imports.
    tx.execute(
        "DELETE FROM type_edges WHERE child_id IN (SELECT id FROM symbols WHERE file = ?1)",
        rusqlite::params![result.rel_path],
    )?;
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

    // Insert new symbols and build a name -> id map for caller_id resolution.
    let mut caller_map: HashMap<&str, i64> = HashMap::new();
    {
        let mut stmt = tx.prepare(
            "INSERT INTO symbols (name, kind, file, line, col, end_line, scope, signature, language, doc_comment) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                sym.doc_comment,
            ])?;
            caller_map.insert(&sym.name, tx.last_insert_rowid());
        }
    }

    // Insert new references, resolving caller_name to caller_id.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO \"references\" (name, file, line, col, context, caller_id, confidence) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for reference in &result.refs {
            let caller_id = reference
                .caller_name
                .as_deref()
                .and_then(|name| caller_map.get(name).copied());
            stmt.execute(rusqlite::params![
                reference.name,
                reference.file,
                reference.line as i64,
                reference.col as i64,
                reference.context,
                caller_id,
                reference.confidence,
            ])?;
        }
    }

    // Insert new imports.
    {
        let mut stmt =
            tx.prepare("INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)")?;
        for import in &result.imports {
            stmt.execute(rusqlite::params![result.rel_path, import])?;
        }
    }

    // Insert type hierarchy edges, resolving names to symbol IDs.
    {
        let mut insert_stmt = tx.prepare(
            "INSERT OR IGNORE INTO type_edges (child_id, parent_id, relationship) \
             VALUES (?1, ?2, ?3)",
        )?;
        let mut cross_file_lookup = tx.prepare("SELECT id FROM symbols WHERE name = ?1 LIMIT 1")?;

        for edge in &result.type_edges {
            // Resolve child_id: must be in the same file.
            let Some(child_id) = caller_map.get(edge.child_name.as_str()).copied() else {
                continue;
            };

            // Resolve parent_id: try same file first, then cross-file.
            let Some(parent_id) =
                caller_map
                    .get(edge.parent_name.as_str())
                    .copied()
                    .or_else(|| {
                        cross_file_lookup
                            .query_row(rusqlite::params![edge.parent_name], |row| {
                                row.get::<_, i64>(0)
                            })
                            .ok()
                    })
            else {
                continue;
            };

            insert_stmt.execute(rusqlite::params![child_id, parent_id, edge.relationship,])?;
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

    // Pre-process Rust source to expand cfg_*! macros.
    let parse_source = if lang == indexer::Lang::Rust {
        indexer::preprocess_rust_macros(&content)
    } else {
        content.clone()
    };

    // Parse with tree-sitter.
    let mut parser = indexer::get_parser(lang);
    let tree = parser.parse(parse_source.as_bytes(), None)?;

    // Relative path for storage.
    let rel_path = path
        .strip_prefix(repo_root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();

    // Extract symbols.
    let symbols = indexer::extract_symbols(&tree, &parse_source, &rel_path, lang);

    // Extract references.
    let mut refs = indexer::extract_references(&tree, &parse_source, &rel_path, lang);

    // Extract imports for dependency graph.
    let file_imports = indexer::extract_imports(&tree, &parse_source, &rel_path, lang);

    // Extract type hierarchy edges (extends/implements).
    let type_edges = indexer::extract_type_edges(&tree, &parse_source, &rel_path, lang);

    // Compute confidence for each reference.
    for r in &mut refs {
        r.confidence = indexer::compute_confidence(r, &symbols, &file_imports.imports);
    }

    let line_count = content.lines().count();

    Some(FileResult {
        rel_path,
        language: lang.name().to_string(),
        content_hash: hash,
        line_count,
        symbols,
        refs,
        imports: file_imports.imports,
        type_edges,
    })
}

/// Insert all results into the database in a single transaction.
///
/// Returns (symbol_count, ref_count, caller_count, type_edge_count).
fn batch_insert(conn: &Connection, results: &[FileResult]) -> Result<(usize, usize, usize, usize)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let tx = conn
        .unchecked_transaction()
        .context("starting transaction")?;

    let mut total_syms = 0usize;
    let mut total_refs = 0usize;
    let mut caller_count = 0usize;

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

    // Insert symbols and build per-file name -> id maps for caller_id resolution.
    let mut file_caller_maps: HashMap<&str, HashMap<&str, i64>> = HashMap::new();
    {
        let mut stmt = tx.prepare(
            "INSERT INTO symbols (name, kind, file, line, col, end_line, scope, signature, language, doc_comment) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        for r in results {
            let file_map = file_caller_maps.entry(&r.rel_path).or_default();
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
                    sym.doc_comment,
                ])?;
                file_map.insert(&sym.name, tx.last_insert_rowid());
                total_syms += 1;
            }
        }
    }

    // Insert references, resolving caller_name to caller_id.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO \"references\" (name, file, line, col, context, caller_id, confidence) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for r in results {
            let file_map = file_caller_maps.get(r.rel_path.as_str());
            for reference in &r.refs {
                let caller_id = reference
                    .caller_name
                    .as_deref()
                    .and_then(|name| file_map?.get(name).copied());
                if caller_id.is_some() {
                    caller_count += 1;
                }
                stmt.execute(rusqlite::params![
                    reference.name,
                    reference.file,
                    reference.line as i64,
                    reference.col as i64,
                    reference.context,
                    caller_id,
                    reference.confidence,
                ])?;
                total_refs += 1;
            }
        }
    }

    // Insert file imports.
    {
        let mut stmt =
            tx.prepare("INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)")?;
        for r in results {
            for import in &r.imports {
                stmt.execute(rusqlite::params![r.rel_path, import])?;
            }
        }
    }

    // Insert type hierarchy edges, resolving names to symbol IDs.
    // Batch-resolve cross-file parent names to avoid N+1 queries.
    let mut type_edge_count = 0usize;
    {
        // Collect parent names that need cross-file resolution.
        let mut unresolved_parents: HashSet<&str> = HashSet::new();
        for r in results.iter() {
            let file_map = file_caller_maps.get(r.rel_path.as_str());
            for edge in &r.type_edges {
                if file_map
                    .and_then(|m| m.get(edge.parent_name.as_str()))
                    .is_none()
                {
                    unresolved_parents.insert(&edge.parent_name);
                }
            }
        }

        // Batch-resolve unresolved parents in a single query per chunk.
        let mut cross_file_map: HashMap<String, i64> = HashMap::new();
        if !unresolved_parents.is_empty() {
            let names: Vec<&str> = unresolved_parents.into_iter().collect();
            // SQLite variable limit is 999; chunk to stay under it.
            for chunk in names.chunks(900) {
                let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "SELECT name, id FROM symbols WHERE name IN ({placeholders}) GROUP BY name"
                );
                let mut stmt = tx.prepare(&sql)?;
                let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                for row in rows {
                    let (name, id) = row?;
                    cross_file_map.insert(name, id);
                }
            }
        }

        let mut insert_stmt = tx.prepare(
            "INSERT OR IGNORE INTO type_edges (child_id, parent_id, relationship) \
             VALUES (?1, ?2, ?3)",
        )?;

        for r in results {
            let file_map = file_caller_maps.get(r.rel_path.as_str());
            for edge in &r.type_edges {
                // Resolve child_id: must be in the same file.
                let Some(child_id) =
                    file_map.and_then(|m| m.get(edge.child_name.as_str()).copied())
                else {
                    continue;
                };

                // Resolve parent_id: try same file first, then cross-file batch map.
                let Some(parent_id) = file_map
                    .and_then(|m| m.get(edge.parent_name.as_str()).copied())
                    .or_else(|| cross_file_map.get(edge.parent_name.as_str()).copied())
                else {
                    continue;
                };

                insert_stmt.execute(rusqlite::params![child_id, parent_id, edge.relationship,])?;
                type_edge_count += 1;
            }
        }
    }

    tx.commit().context("committing transaction")?;
    Ok((total_syms, total_refs, caller_count, type_edge_count))
}

// ---------------------------------------------------------------------------
// Embedding build pipeline
// ---------------------------------------------------------------------------

/// Statistics returned after an embedding build run.
#[derive(Debug, Clone)]
pub struct EmbeddingBuildStats {
    /// Number of symbols successfully embedded.
    pub embedded_count: usize,
    /// Total number of symbol chunks generated.
    pub total_symbols: usize,
    /// Whether the entire embedding pass was skipped (Ollama unreachable).
    pub skipped: bool,
    /// Wall-clock elapsed time.
    pub elapsed: std::time::Duration,
}

/// Batch size for Ollama API calls.
const EMBEDDING_BATCH_SIZE: usize = 50;

/// How the batch-embed loop handles errors from Ollama.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EmbedErrorPolicy {
    /// Log and break — partial results are kept (used by `wonk init`).
    SkipPartial,
    /// Return `Err` immediately — caller cannot proceed without all
    /// embeddings (used by `wonk ask`).
    FailFast,
}

/// Handle an embedding interruption according to the error policy.
///
/// With [`EmbedErrorPolicy::FailFast`], returns `Err` so the `?` operator
/// propagates the failure.  With [`EmbedErrorPolicy::SkipPartial`], logs the
/// message (unless silent) and returns `Ok(())` — the caller should `break`.
fn handle_embed_interruption(msg: &str, policy: EmbedErrorPolicy, silent: bool) -> Result<()> {
    match policy {
        EmbedErrorPolicy::FailFast => anyhow::bail!("{msg}"),
        EmbedErrorPolicy::SkipPartial => {
            if !silent {
                eprintln!("{msg}");
            }
            Ok(())
        }
    }
}

/// Retry a failed batch by embedding each text individually.
///
/// When a batch fails with a context-length error, this function retries each
/// text one by one.  Texts that embed successfully are stored normally; texts
/// that hit the context-length limit again are skipped with a log message.
/// Returns `(embedded_count, should_break)`.
fn embed_batch_individually(
    conn: &Connection,
    batch: &[(i64, String, String)],
    client: &OllamaClient,
    policy: EmbedErrorPolicy,
    silent: bool,
) -> Result<(usize, bool)> {
    let mut count = 0usize;
    for (sym_id, file, text) in batch {
        match client.embed_single(text) {
            Ok(vec) => {
                embedding::store_embeddings_batch(
                    conn,
                    &[(*sym_id, file.as_str(), text.as_str(), vec.as_slice())],
                )?;
                count += 1;
            }
            Err(ref e) if embedding::is_context_length_error(e) => {
                if !silent {
                    eprintln!("Skipping oversized symbol (id={sym_id}, file={file})");
                }
            }
            Err(EmbeddingError::OllamaUnreachable) => {
                handle_embed_interruption(
                    "Ollama became unreachable during individual retry",
                    policy,
                    silent,
                )?;
                return Ok((count, true));
            }
            Err(e) => {
                handle_embed_interruption(
                    &format!("Embedding error during individual retry: {e}"),
                    policy,
                    silent,
                )?;
                return Ok((count, true));
            }
        }
    }
    Ok((count, false))
}

/// Shared batch-embed loop.
///
/// Iterates over `chunks` in groups of [`EMBEDDING_BATCH_SIZE`], embeds each
/// batch via `client`, and stores the resulting vectors.  Returns the number
/// of successfully embedded symbols.
fn embed_chunks(
    conn: &Connection,
    chunks: &[(i64, String, String)],
    client: &OllamaClient,
    progress_mode: ProgressMode,
    policy: EmbedErrorPolicy,
) -> Result<usize> {
    let total = chunks.len();
    let silent = progress_mode == ProgressMode::Silent;
    let mut embedded = 0usize;

    for batch_start in (0..total).step_by(EMBEDDING_BATCH_SIZE) {
        let batch_end = (batch_start + EMBEDDING_BATCH_SIZE).min(total);
        let batch = &chunks[batch_start..batch_end];

        let texts: Vec<String> = batch.iter().map(|(_, _, text)| text.clone()).collect();

        let vectors = match client.embed_batch(&texts) {
            Ok(v) => v,
            Err(EmbeddingError::OllamaUnreachable) => {
                let msg = format!(
                    "Ollama became unreachable after embedding {embedded}/{total} symbols."
                );
                handle_embed_interruption(&msg, policy, silent)?;
                break;
            }
            Err(ref e) if embedding::is_context_length_error(e) => {
                // A chunk in the batch exceeds context length — retry individually.
                if !silent {
                    eprintln!("Batch context-length error; retrying individually...");
                }
                let (fallback_count, should_break) =
                    embed_batch_individually(conn, batch, client, policy, silent)?;
                embedded += fallback_count;
                if should_break {
                    break;
                }
                render_embedding_progress(progress_mode, embedded, total);
                continue;
            }
            Err(e) => {
                let msg =
                    format!("Embedding error: {e}. Stopped after {embedded}/{total} symbols.");
                handle_embed_interruption(&msg, policy, silent)?;
                break;
            }
        };

        // Validate response count matches request count.
        if vectors.len() != texts.len() {
            let msg = format!(
                "Ollama returned {} vectors for {} texts. Stopped after {embedded}/{total} symbols.",
                vectors.len(),
                texts.len(),
            );
            handle_embed_interruption(&msg, policy, silent)?;
            break;
        }

        // Build storage tuples.
        let store_batch: Vec<(i64, &str, &str, &[f32])> = batch
            .iter()
            .zip(vectors.iter())
            .map(|((sym_id, file, text), vec)| {
                (*sym_id, file.as_str(), text.as_str(), vec.as_slice())
            })
            .collect();

        embedding::store_embeddings_batch(conn, &store_batch).context("storing embedding batch")?;

        embedded += store_batch.len();

        render_embedding_progress(progress_mode, embedded, total);
    }

    // Clear the progress line if in-place mode.
    if progress_mode == ProgressMode::InPlace && embedded > 0 {
        eprintln!("\rEmbedded {embedded}/{total} symbols{:<40}", "");
    }

    Ok(embedded)
}

/// Build embeddings for all indexed symbols.
///
/// Checks Ollama health first; if unreachable, returns with `skipped = true`.
/// Otherwise generates chunks, deletes existing embeddings, and batch-embeds
/// in groups of [`EMBEDDING_BATCH_SIZE`].  Partial failures (Ollama going down
/// mid-batch) are handled gracefully: previously committed batches are persisted.
pub fn build_embeddings(
    conn: &Connection,
    repo_root: &Path,
    client: &OllamaClient,
    progress_mode: ProgressMode,
) -> Result<EmbeddingBuildStats> {
    let start = Instant::now();

    // Health check.
    if !client.is_healthy() {
        if progress_mode != ProgressMode::Silent {
            eprintln!(
                "Ollama not available — skipping embedding generation. \
                 Semantic search will not be available until embeddings are built."
            );
        }
        return Ok(EmbeddingBuildStats {
            embedded_count: 0,
            total_symbols: 0,
            skipped: true,
            elapsed: start.elapsed(),
        });
    }

    // Generate chunks.
    let chunks =
        embedding::chunk_all_symbols(conn, repo_root).context("chunking symbols for embedding")?;

    if chunks.is_empty() {
        return Ok(EmbeddingBuildStats {
            embedded_count: 0,
            total_symbols: 0,
            skipped: false,
            elapsed: start.elapsed(),
        });
    }

    let total = chunks.len();

    // Delete existing embeddings for a clean rebuild.
    conn.execute("DELETE FROM embeddings", [])
        .context("clearing old embeddings")?;

    let embedded = embed_chunks(
        conn,
        &chunks,
        client,
        progress_mode,
        EmbedErrorPolicy::SkipPartial,
    )?;

    Ok(EmbeddingBuildStats {
        embedded_count: embedded,
        total_symbols: total,
        skipped: false,
        elapsed: start.elapsed(),
    })
}

/// Build embeddings only for symbols that lack fresh (non-stale) embeddings.
///
/// Unlike [`build_embeddings`], this does **not** delete existing embeddings
/// first -- it is incremental.  Returns `Err` when Ollama is unreachable
/// (the caller needs Ollama for the subsequent query).
pub fn build_missing_embeddings(
    conn: &Connection,
    repo_root: &Path,
    client: &OllamaClient,
    progress_mode: ProgressMode,
) -> Result<EmbeddingBuildStats> {
    let start = Instant::now();

    // Generate chunks only for un-embedded / stale symbols.
    let chunks = embedding::chunk_missing_symbols(conn, repo_root)
        .context("chunking missing symbols for embedding")?;

    if chunks.is_empty() {
        return Ok(EmbeddingBuildStats {
            embedded_count: 0,
            total_symbols: 0,
            skipped: false,
            elapsed: start.elapsed(),
        });
    }

    let total = chunks.len();

    // Health check — bail before starting the expensive batch-embed loop.
    // Unlike build_embeddings we return Err because the caller (wonk ask)
    // requires Ollama.
    if !client.is_healthy() {
        anyhow::bail!("{}", embedding::OLLAMA_REQUIRED_MSG);
    }

    let embedded = embed_chunks(
        conn,
        &chunks,
        client,
        progress_mode,
        EmbedErrorPolicy::FailFast,
    )?;

    Ok(EmbeddingBuildStats {
        embedded_count: embedded,
        total_symbols: total,
        skipped: false,
        elapsed: start.elapsed(),
    })
}

/// Render embedding progress to stderr.
fn render_embedding_progress(mode: ProgressMode, done: usize, total: usize) {
    match mode {
        ProgressMode::Silent => {}
        ProgressMode::InPlace => {
            eprint!("\rEmbedding... [{done}/{total} symbols]");
        }
        ProgressMode::LineBased => {
            eprintln!("Embedding... [{done}/{total} symbols]");
        }
    }
}

/// Re-embed symbols for files that have changed during incremental re-indexing.
///
/// If Ollama is healthy:
///   1. Delete old embeddings for each changed file.
///   2. Generate chunks for symbols in those files.
///   3. Embed via Ollama and store new vectors.
///
/// If Ollama is unhealthy:
///   Mark embeddings stale for each changed file so they are picked up on
///   the next full embedding pass.
///
/// Returns the number of symbols successfully embedded (0 if Ollama was
/// unreachable or the file list was empty).
pub fn reembed_changed_files(
    conn: &Connection,
    repo_root: &Path,
    changed_files: &[String],
    client: &OllamaClient,
) -> Result<usize> {
    if changed_files.is_empty() {
        return Ok(0);
    }

    if !client.is_healthy() {
        // Ollama unreachable: mark embeddings stale for each file in a single transaction.
        let tx = conn
            .unchecked_transaction()
            .context("starting stale-mark transaction")?;
        for file in changed_files {
            embedding::mark_embeddings_stale(&tx, file).context("marking embeddings stale")?;
        }
        tx.commit().context("committing stale-mark transaction")?;
        return Ok(0);
    }

    // Delete old embeddings for changed files in a single transaction.
    let tx = conn
        .unchecked_transaction()
        .context("starting delete-embeddings transaction")?;
    for file in changed_files {
        embedding::delete_embeddings_for_file(&tx, file)
            .context("deleting embeddings for changed file")?;
    }
    tx.commit()
        .context("committing delete-embeddings transaction")?;

    // Generate chunks for the changed files.
    let chunks = embedding::chunk_symbols_for_files(conn, repo_root, changed_files)
        .context("chunking symbols for changed files")?;

    if chunks.is_empty() {
        return Ok(0);
    }

    // Embed and store (SkipPartial: daemon should not crash on embedding failures).
    let embedded = embed_chunks(
        conn,
        &chunks,
        client,
        ProgressMode::Silent,
        EmbedErrorPolicy::SkipPartial,
    )?;

    Ok(embedded)
}

/// Drop all data from the main tables (used before rebuild).
fn drop_all_data(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM embeddings;
         DELETE FROM type_edges;
         DELETE FROM symbols;
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

        assert!(
            stats.file_count >= 3,
            "should index at least 3 files, got {}",
            stats.file_count
        );
        assert!(stats.symbol_count > 0, "should extract symbols");
        // ref_count is usize so it's always >= 0; just ensure indexing ran.
        let _ = stats.ref_count;
        assert!(stats.elapsed.as_nanos() > 0, "elapsed should be positive");
    }

    #[test]
    fn test_build_index_caller_count() {
        // The test repo has src/main.rs with fn main() calling helper(),
        // so there should be at least one resolved caller_id relationship.
        let dir = make_test_repo();
        let stats = build_index(dir.path(), true).unwrap();

        assert!(
            stats.caller_count > 0,
            "should have caller relationships, got {}",
            stats.caller_count
        );
        assert!(
            stats.caller_count <= stats.ref_count,
            "caller_count ({}) should not exceed ref_count ({})",
            stats.caller_count,
            stats.ref_count
        );
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
        assert!(
            file_count >= 3,
            "files table should have at least 3 entries"
        );

        // Check that files have hashes.
        let hash: String = conn
            .query_row("SELECT hash FROM files LIMIT 1", [], |row| row.get(0))
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
            .query_row("SELECT hash FROM files WHERE path = 'test.rs'", [], |row| {
                row.get(0)
            })
            .unwrap();

        // Modify the file and rebuild.
        fs::write(dir.path().join("test.rs"), "fn foo() { 42 }").unwrap();
        let _stats2 = rebuild_index(dir.path(), true).unwrap();
        let conn2 = db::open_existing(&index_path).unwrap();
        let hash2: String = conn2
            .query_row("SELECT hash FROM files WHERE path = 'test.rs'", [], |row| {
                row.get(0)
            })
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
            .query_row("SELECT hash FROM files WHERE path = 'lib.rs'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let orig_sym_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
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
            .query_row("SELECT hash FROM files WHERE path = 'lib.rs'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_ne!(orig_hash, new_hash, "hash should change after modification");

        // Symbol count should have increased (we added a function).
        let new_sym_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
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
            .query_row(
                "SELECT last_indexed FROM files WHERE path = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // Change the file.
        fs::write(root.join("lib.rs"), "fn only_one() {}").unwrap();
        let changed = reindex_file(&conn, &root.join("lib.rs"), root).unwrap();
        assert!(changed);

        // last_indexed should be updated.
        let new_indexed: i64 = conn
            .query_row(
                "SELECT last_indexed FROM files WHERE path = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            new_indexed >= orig_indexed,
            "last_indexed should be updated"
        );

        // symbols_count should reflect the new file content.
        let sym_count_meta: i64 = conn
            .query_row(
                "SELECT symbols_count FROM files WHERE path = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let sym_count_actual: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            sym_count_meta, sym_count_actual,
            "symbols_count metadata should match actual count"
        );
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
        assert_eq!(
            has_hello_after, 0,
            "'hello' symbol should be removed after re-index"
        );

        // New symbols should be present.
        let has_alpha: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs' AND name = 'alpha'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            has_alpha > 0,
            "'alpha' symbol should be present after re-index"
        );
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
        assert_eq!(
            fts_hello_after, 0,
            "FTS should not contain 'hello' after re-index"
        );

        // 'replacement' should be in FTS.
        let fts_replacement: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'replacement'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            fts_replacement > 0,
            "FTS should contain 'replacement' after re-index"
        );
    }

    #[test]
    fn test_remove_file_deletes_all_data() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Verify data exists before removal.
        let file_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(file_count, 1);

        let sym_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sym_count > 0);

        // Remove the file from the index.
        remove_file(&conn, &root.join("lib.rs"), root).unwrap();

        // All data should be gone.
        let file_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(file_count_after, 0, "files row should be removed");

        let sym_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sym_count_after, 0, "symbols should be removed");

        let ref_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM \"references\" WHERE file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
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
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'app.py'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(py_file, 1, "app.py should still be in the index");

        let py_syms: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'app.py'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(py_syms > 0, "app.py symbols should still be in the index");
    }

    #[test]
    fn test_index_new_file() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Create a new file not yet in the index.
        fs::write(
            root.join("new_file.rs"),
            "fn brand_new() {}\nstruct Fresh {}",
        )
        .unwrap();

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
    fn test_process_events_returns_changed_files() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Modify an existing file.
        fs::write(root.join("lib.rs"), "fn modified_func() {}").unwrap();

        // Create a new file.
        fs::write(root.join("extra.rs"), "fn extra() {}").unwrap();

        let events = vec![
            FileEvent::Modified(root.join("lib.rs")),
            FileEvent::Created(root.join("extra.rs")),
            FileEvent::Deleted(root.join("app.py")),
        ];

        let result = process_events(&conn, &events, root).unwrap();

        // ProcessResult should report the count and the changed file paths.
        assert_eq!(result.updated_count, 3);
        assert_eq!(result.changed_files.len(), 3);
        assert!(result.changed_files.contains(&"lib.rs".to_string()));
        assert!(result.changed_files.contains(&"extra.rs".to_string()));
        assert!(result.changed_files.contains(&"app.py".to_string()));
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

        let result = process_events(&conn, &events, root).unwrap();
        // All three should count as updates (modify changed hash, new file, delete).
        assert_eq!(
            result.updated_count, 3,
            "all three events should result in updates"
        );

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
        let result = process_events(&conn, &events, dir.path()).unwrap();
        assert_eq!(
            result.updated_count, 0,
            "empty batch should produce 0 updates"
        );
        assert!(result.changed_files.is_empty());
    }

    #[test]
    fn test_process_events_unchanged_file() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Send a Modified event for a file that hasn't actually changed.
        let events = vec![FileEvent::Modified(root.join("lib.rs"))];
        let result = process_events(&conn, &events, root).unwrap();
        assert_eq!(
            result.updated_count, 0,
            "unchanged file should not count as updated"
        );
        assert!(result.changed_files.is_empty());
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

        let result = process_events(&conn, &events, root).unwrap();
        // The ghost.rs error should not prevent lib.rs from being processed.
        assert_eq!(
            result.updated_count, 1,
            "should process remaining events after error"
        );

        let has_changed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs' AND name = 'changed_after_error'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            has_changed > 0,
            "lib.rs should be re-indexed despite earlier error"
        );
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
        assert_eq!(
            progress.done(),
            progress.total(),
            "all files should be processed"
        );
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

        let progress = Arc::new(Progress::new(
            "Re-indexing",
            "Re-indexed",
            ProgressMode::Silent,
        ));
        let stats2 = rebuild_index_with_progress(dir.path(), true, &progress).unwrap();

        assert!(
            progress.total() > 0,
            "progress total should be set for rebuild"
        );
        assert_eq!(
            progress.done(),
            progress.total(),
            "all files processed in rebuild"
        );
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
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // Modify the file slightly (same symbols, different content to change hash).
        fs::write(
            root.join("lib.rs"),
            "fn hello() { 1 }\nfn world() { 2 }\n// comment",
        )
        .unwrap();
        reindex_file(&conn, &root.join("lib.rs"), root).unwrap();

        let new_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // Should be the same number (old symbols deleted, new ones inserted).
        assert_eq!(
            orig_count, new_count,
            "symbol count should not double after re-index: orig={orig_count}, new={new_count}"
        );
    }

    // -----------------------------------------------------------------------
    // Embedding pipeline tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_embedding_build_stats_struct() {
        let stats = EmbeddingBuildStats {
            embedded_count: 10,
            total_symbols: 20,
            skipped: false,
            elapsed: std::time::Duration::from_secs(1),
        };
        assert_eq!(stats.embedded_count, 10);
        assert_eq!(stats.total_symbols, 20);
        assert!(!stats.skipped);
    }

    #[test]
    fn test_build_embeddings_ollama_unreachable_skips() {
        let dir = make_test_repo();
        let root = dir.path();
        let _stats = build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        // Use a dead port to simulate Ollama unreachable.
        let client = crate::embedding::OllamaClient::with_base_url("http://127.0.0.1:19999");
        let progress_mode = crate::progress::ProgressMode::Silent;

        let emb_stats = build_embeddings(&conn, root, &client, progress_mode).unwrap();
        assert!(emb_stats.skipped, "should skip when Ollama is unreachable");
        assert_eq!(emb_stats.embedded_count, 0);
    }

    #[test]
    fn test_drop_all_data_clears_embeddings() {
        let dir = make_test_repo();
        let root = dir.path();
        let _stats = build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        // Insert a fake embedding.
        let sym_id: i64 = conn
            .query_row("SELECT id FROM symbols LIMIT 1", [], |row| row.get(0))
            .unwrap();
        crate::embedding::store_embedding(&conn, sym_id, "test.rs", "chunk", &[1.0, 0.0]).unwrap();

        let (total, _) = crate::embedding::embedding_stats(&conn).unwrap();
        assert_eq!(total, 1, "should have 1 embedding before drop");

        drop_all_data(&conn).unwrap();

        let (total_after, _) = crate::embedding::embedding_stats(&conn).unwrap();
        assert_eq!(
            total_after, 0,
            "embeddings should be cleared after drop_all_data"
        );
    }

    // -----------------------------------------------------------------------
    // build_missing_embeddings tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_missing_embeddings_ollama_unreachable_returns_error() {
        let dir = make_test_repo();
        let root = dir.path();
        let _stats = build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        // Use a dead port to simulate Ollama unreachable.
        let client = crate::embedding::OllamaClient::with_base_url("http://127.0.0.1:19999");
        let progress_mode = crate::progress::ProgressMode::Silent;

        let result = build_missing_embeddings(&conn, root, &client, progress_mode);
        assert!(result.is_err(), "should return Err when Ollama unreachable");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Ollama"),
            "error should mention Ollama: {}",
            err
        );
    }

    #[test]
    fn test_build_missing_embeddings_no_symbols_returns_ok() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        // Empty repo - no source files.
        let _stats = build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        let client = crate::embedding::OllamaClient::with_base_url("http://127.0.0.1:19999");
        let progress_mode = crate::progress::ProgressMode::Silent;

        let result = build_missing_embeddings(&conn, root, &client, progress_mode);
        assert!(result.is_ok(), "should succeed with no symbols");
        let stats = result.unwrap();
        assert_eq!(stats.embedded_count, 0);
        assert_eq!(stats.total_symbols, 0);
        assert!(!stats.skipped);
    }

    // -----------------------------------------------------------------------
    // reembed_changed_files tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_reembed_changed_files_ollama_unreachable_marks_stale() {
        let dir = make_test_repo();
        let root = dir.path();
        let _stats = build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        // Store a fake embedding for one of the symbols in src/main.rs.
        let sym_id: i64 = conn
            .query_row(
                "SELECT id FROM symbols WHERE file = 'src/main.rs' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        embedding::store_embedding(&conn, sym_id, "src/main.rs", "chunk", &[1.0, 0.0]).unwrap();

        // Verify embedding is fresh (not stale).
        let (_, stale_before) = embedding::embedding_stats(&conn).unwrap();
        assert_eq!(stale_before, 0, "embedding should be fresh initially");

        // Use dead port to simulate Ollama unreachable.
        let client = embedding::OllamaClient::with_base_url("http://127.0.0.1:19999");
        let files = vec!["src/main.rs".to_string()];

        let count = reembed_changed_files(&conn, root, &files, &client).unwrap();
        assert_eq!(count, 0, "should embed 0 when Ollama is unreachable");

        // Embedding should now be stale.
        let (_, stale_after) = embedding::embedding_stats(&conn).unwrap();
        assert_eq!(stale_after, 1, "embedding should be marked stale");
    }

    #[test]
    fn test_reembed_changed_files_empty_list_is_noop() {
        let dir = make_test_repo();
        let root = dir.path();
        let _stats = build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        let client = embedding::OllamaClient::with_base_url("http://127.0.0.1:19999");
        let files: Vec<String> = vec![];

        let count = reembed_changed_files(&conn, root, &files, &client).unwrap();
        assert_eq!(count, 0, "empty file list should be noop");
    }

    #[test]
    fn test_reembed_changed_files_deleted_file_skipped() {
        let dir = make_test_repo();
        let root = dir.path();
        let _stats = build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        // Use dead port; the function should mark stale rather than error.
        let client = embedding::OllamaClient::with_base_url("http://127.0.0.1:19999");
        // File that was deleted from disk but still referenced.
        let files = vec!["nonexistent.rs".to_string()];

        let count = reembed_changed_files(&conn, root, &files, &client).unwrap();
        assert_eq!(count, 0, "deleted file should not produce embeddings");
    }

    // -- Confidence scoring integration tests -----------------------------------

    #[test]
    fn test_index_stores_confidence_values() {
        // Build an index with a Rust file that has same-file calls and imports.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"use std::io;

fn main() {
    helper();
}

fn helper() -> i32 {
    42
}
"#,
        )
        .unwrap();

        build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        // "helper" is called from within the same file where it's defined,
        // so its reference should have confidence > 0.5.
        let confidence: f64 = conn
            .query_row(
                "SELECT confidence FROM \"references\" WHERE name = 'helper'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            confidence > 0.5,
            "same-file reference to 'helper' should have confidence > 0.5, got {confidence}"
        );

        // Import references (e.g. "io" from "use std::io") should have confidence 0.95.
        let import_conf: Option<f64> = conn
            .query_row(
                "SELECT confidence FROM \"references\" WHERE name = 'io' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();
        if let Some(c) = import_conf {
            assert!(
                c >= 0.9,
                "import reference should have confidence >= 0.9, got {c}"
            );
        }
    }

    #[test]
    fn test_upsert_preserves_confidence() {
        // Verify that incremental re-index via upsert_file_data also stores confidence.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "fn caller() {\n    callee();\n}\n\nfn callee() {}\n",
        )
        .unwrap();

        build_index(root, true).unwrap();

        // Modify the file and re-index.
        fs::write(
            root.join("src/lib.rs"),
            "fn caller() {\n    callee();\n    callee();\n}\n\nfn callee() {}\n",
        )
        .unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        reindex_file(&conn, &root.join("src/lib.rs"), root).unwrap();

        // Check that confidence is stored for the re-indexed reference.
        let confidence: f64 = conn
            .query_row(
                "SELECT confidence FROM \"references\" WHERE name = 'callee' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            confidence > 0.5,
            "same-file reference after upsert should have confidence > 0.5, got {confidence}"
        );
    }

    // -- type_edges pipeline integration tests ---------------------------------

    #[test]
    fn test_build_index_type_edges() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();

        // TypeScript file with class hierarchy.
        fs::write(
            root.join("app.ts"),
            r#"class Animal {}
class Dog extends Animal {}
interface Runnable { run(): void; }
class Worker implements Runnable { run() {} }
"#,
        )
        .unwrap();

        let stats = build_index(root, true).unwrap();
        assert!(stats.symbol_count > 0);
        assert!(
            stats.type_edge_count > 0,
            "should have type edges, got {}",
            stats.type_edge_count
        );

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        let edge_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM type_edges", [], |row| row.get(0))
            .unwrap();
        assert!(
            edge_count >= 2,
            "should have at least 2 type edges (extends + implements), got {edge_count}"
        );

        // Verify specific edges exist.
        let extends_rel: String = conn
            .query_row(
                "SELECT te.relationship FROM type_edges te \
                 JOIN symbols child ON te.child_id = child.id \
                 JOIN symbols parent ON te.parent_id = parent.id \
                 WHERE child.name = 'Dog' AND parent.name = 'Animal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(extends_rel, "extends");

        let impl_rel: String = conn
            .query_row(
                "SELECT te.relationship FROM type_edges te \
                 JOIN symbols child ON te.child_id = child.id \
                 JOIN symbols parent ON te.parent_id = parent.id \
                 WHERE child.name = 'Worker' AND parent.name = 'Runnable'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(impl_rel, "implements");
    }

    #[test]
    fn test_reindex_file_updates_type_edges() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();

        // Write a TypeScript file with class hierarchy: Dog extends Animal.
        fs::write(
            root.join("app.ts"),
            "class Animal {}\nclass Dog extends Animal {}\n",
        )
        .unwrap();

        let _stats = build_index(root, true).unwrap();
        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        // Verify initial type_edge exists (Dog -> Animal, extends).
        let initial_edges: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM type_edges te \
                 JOIN symbols child ON te.child_id = child.id \
                 JOIN symbols parent ON te.parent_id = parent.id \
                 WHERE child.name = 'Dog' AND parent.name = 'Animal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(initial_edges, 1, "should have Dog->Animal edge initially");

        // Modify file: change Dog to extend Creature instead of Animal.
        fs::write(
            root.join("app.ts"),
            "class Creature {}\nclass Dog extends Creature {}\n",
        )
        .unwrap();

        let changed = reindex_file(&conn, &root.join("app.ts"), root).unwrap();
        assert!(changed, "modified file should be re-indexed");

        // Old edge (Dog -> Animal) should be gone.
        let old_edge: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM type_edges te \
                 JOIN symbols child ON te.child_id = child.id \
                 JOIN symbols parent ON te.parent_id = parent.id \
                 WHERE child.name = 'Dog' AND parent.name = 'Animal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_edge, 0, "old Dog->Animal edge should be removed");

        // New edge (Dog -> Creature) should exist.
        let new_edge: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM type_edges te \
                 JOIN symbols child ON te.child_id = child.id \
                 JOIN symbols parent ON te.parent_id = parent.id \
                 WHERE child.name = 'Dog' AND parent.name = 'Creature'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_edge, 1, "new Dog->Creature edge should exist");
    }

    #[test]
    fn test_remove_file_deletes_type_edges() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();

        // Write a TypeScript file with class hierarchy.
        fs::write(
            root.join("app.ts"),
            "class Animal {}\nclass Dog extends Animal {}\n",
        )
        .unwrap();

        let stats = build_index(root, true).unwrap();
        assert!(
            stats.type_edge_count > 0,
            "should have type edges after build"
        );

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        // Verify type_edges exist before removal.
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM type_edges", [], |row| row.get(0))
            .unwrap();
        assert!(before > 0, "should have type edges before removal");

        // Remove the file.
        remove_file(&conn, &root.join("app.ts"), root).unwrap();

        // Type edges should be gone.
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM type_edges", [], |row| row.get(0))
            .unwrap();
        assert_eq!(after, 0, "type edges should be removed after file removal");
    }

    #[test]
    fn test_rebuild_index_recalculates_confidence_and_type_edges() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();

        // TypeScript file with class hierarchy and function calls.
        // Using import to get 0.95 confidence and same-file def for 0.85.
        fs::write(
            root.join("app.ts"),
            r#"import { helper } from './util';
class Animal {}
class Dog extends Animal {}
function greet() { return helper(); }
function unknown() { return mystery(); }
"#,
        )
        .unwrap();

        // Build initial index.
        let stats1 = build_index(root, true).unwrap();
        assert!(stats1.type_edge_count > 0, "should have type edges");

        let index_path = db::local_index_path(root);
        let conn1 = db::open_existing(&index_path).unwrap();

        let edges_before: i64 = conn1
            .query_row("SELECT COUNT(*) FROM type_edges", [], |row| row.get(0))
            .unwrap();
        assert!(edges_before > 0, "should have type edges before rebuild");

        // Verify confidence is NOT all default 0.5 (we have import-resolved refs).
        let has_non_default: i64 = conn1
            .query_row(
                "SELECT COUNT(*) FROM \"references\" WHERE ABS(confidence - 0.5) > 0.01",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            has_non_default > 0,
            "should have non-default confidence values before rebuild, got {}",
            has_non_default
        );
        drop(conn1);

        // Rebuild from scratch.
        let stats2 = rebuild_index(root, true).unwrap();
        assert!(
            stats2.type_edge_count > 0,
            "should have type edges after rebuild"
        );

        let conn2 = db::open_existing(&index_path).unwrap();

        let edges_after: i64 = conn2
            .query_row("SELECT COUNT(*) FROM type_edges", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            edges_before, edges_after,
            "rebuild should preserve same type edge count"
        );

        // Confidence should still be non-default after rebuild.
        let has_non_default2: i64 = conn2
            .query_row(
                "SELECT COUNT(*) FROM \"references\" WHERE ABS(confidence - 0.5) > 0.01",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            has_non_default2 > 0,
            "should still have non-default confidence after rebuild, got {}",
            has_non_default2
        );
    }

    #[test]
    fn test_process_events_handles_type_edges() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();

        // TypeScript file with class hierarchy.
        fs::write(
            root.join("app.ts"),
            "class Animal {}\nclass Dog extends Animal {}\n",
        )
        .unwrap();

        let _stats = build_index(root, true).unwrap();
        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        // Verify initial type_edges.
        let edges_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM type_edges", [], |row| row.get(0))
            .unwrap();
        assert!(edges_before > 0, "should have type edges initially");

        // Modify file: change class hierarchy.
        fs::write(
            root.join("app.ts"),
            "class Creature {}\nclass Cat extends Creature {}\n",
        )
        .unwrap();

        let events = vec![FileEvent::Modified(root.join("app.ts"))];
        let result = process_events(&conn, &events, root).unwrap();
        assert_eq!(result.updated_count, 1);

        // Old edges (Dog -> Animal) should be gone.
        let old_edge: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM type_edges te \
                 JOIN symbols child ON te.child_id = child.id \
                 WHERE child.name = 'Dog'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_edge, 0, "old Dog edge should be removed");

        // New edges (Cat -> Creature) should exist.
        let new_edge: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM type_edges te \
                 JOIN symbols child ON te.child_id = child.id \
                 JOIN symbols parent ON te.parent_id = parent.id \
                 WHERE child.name = 'Cat' AND parent.name = 'Creature'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_edge, 1, "new Cat->Creature edge should exist");
    }

    #[test]
    fn test_build_index_type_edges_unresolvable() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();

        // TypeScript file where parent class is not defined in any indexed file.
        fs::write(
            root.join("child.ts"),
            "class Child extends UnknownParent {}\n",
        )
        .unwrap();

        let stats = build_index(root, true).unwrap();
        assert!(stats.symbol_count > 0);
        assert_eq!(
            stats.type_edge_count, 0,
            "unresolvable parent should produce no type edges"
        );

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        let edge_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM type_edges", [], |row| row.get(0))
            .unwrap();
        assert_eq!(edge_count, 0);
    }
}
