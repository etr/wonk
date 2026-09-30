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
use crate::embedding::{self, EmbeddingProvider};
use crate::errors::EmbeddingError;
use crate::indexer;
use crate::progress::{Progress, ProgressMode};
use crate::types::{ContractCandidate, RawTypeEdge, Reference, Symbol};
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
    /// Contract rows persisted in the `contracts` table (TASK-083).
    pub contract_count: usize,
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
    /// BM25 term frequencies over the raw content (TASK-078).
    term_freqs: HashMap<String, u32>,
    /// Contract candidates extracted on the same tree walk (TASK-082).
    contracts: Vec<ContractCandidate>,
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
    let contract_opts = crate::contracts::ContractOptions::from(&config.contracts);
    let results: Vec<FileResult> = paths
        .par_iter()
        .filter_map(|path| {
            let result = parse_one_file(path, repo_root, &contract_opts);
            progress.inc();
            result
        })
        .collect();

    // 5. Batch insert (reach table built in the same transaction when enabled).
    let reach_opts = if config.reach.enabled {
        Some(crate::reach::ReachBuildOptions {
            depth: config.reach.depth.min(crate::blast::MAX_DEPTH),
            ..Default::default()
        })
    } else {
        None
    };
    let (sym_count, ref_count, caller_count, type_edge_count, contract_count) =
        batch_insert(&conn, &results, reach_opts.as_ref())?;

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
        contract_count,
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
    let contract_opts = crate::contracts::ContractOptions::from(&config.contracts);
    for rel in &on_disk {
        let abs = repo_root.join(rel);
        let _ = reindex_file(&conn, &abs, repo_root, &contract_opts);
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
    let contract_count = conn
        .query_row("SELECT COUNT(*) FROM contracts", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize;

    Ok(IndexStats {
        file_count,
        symbol_count,
        ref_count,
        caller_count,
        type_edge_count,
        contract_count,
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
pub fn reindex_file(
    conn: &Connection,
    file_path: &Path,
    repo_root: &Path,
    contract_opts: &crate::contracts::ContractOptions,
) -> Result<bool> {
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

    // Detect language — if unsupported, try the document path, then remove
    // stale data and return.
    let lang = match indexer::detect_language(file_path) {
        Some(l) => l,
        None => {
            // Document files (.proto/.graphql/.yaml/.json) carry contracts
            // without a grammar (TASK-088); re-index them, and drop any
            // stale row when they no longer yield candidates. Lock files
            // and oversized documents are never scanned.
            let doc = crate::contracts::scannable_document_kind(file_path).and_then(|kind| {
                document_file_result(
                    kind,
                    rel_path.clone(),
                    &content,
                    new_hash.clone(),
                    contract_opts,
                )
            });
            match doc {
                Some(result) => {
                    upsert_file_data(conn, &result)?;
                    return Ok(true);
                }
                None => {
                    delete_file_data(conn, &rel_path)?;
                    return Ok(false);
                }
            }
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

    // Extract contract candidates on the same tree (PRD-CTR-REQ-011).
    let contracts = crate::contracts::extract_contracts(&tree, &parse_source, lang, contract_opts);

    // Compute confidence for each reference.
    for r in &mut refs {
        r.confidence = indexer::compute_confidence(r, &symbols, &file_imports.imports);
    }

    let line_count = content.lines().count();
    let term_freqs = crate::tokenizer::term_frequencies(&content);

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
            term_freqs,
            contracts,
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
pub fn index_new_file(
    conn: &Connection,
    file_path: &Path,
    repo_root: &Path,
    contract_opts: &crate::contracts::ContractOptions,
) -> Result<()> {
    // Delegate to reindex_file — it handles the "not yet in index" case
    // identically to "hash changed" (the stored hash will be None, so the
    // comparison will always trigger a full index).
    let _ = reindex_file(conn, file_path, repo_root, contract_opts)?;
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
    contract_opts: &crate::contracts::ContractOptions,
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
            FileEvent::Created(path) => {
                index_new_file(conn, path, repo_root, contract_opts).map(|()| true)
            }
            FileEvent::Modified(path) => reindex_file(conn, path, repo_root, contract_opts),
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

    // Capture the reach table's pre-delete view of this file before any
    // rows go away (TASK-081, PRD-REACH-REQ-005).
    let scope = crate::reach::begin_file_edit(&tx, rel_path)?;

    // Delete type edges before symbols (explicit, mirrors references/imports pattern).
    tx.execute(
        "DELETE FROM type_edges WHERE child_id IN (SELECT id FROM symbols WHERE file = ?1)",
        rusqlite::params![rel_path],
    )?;
    // Contracts carry a NULL symbol_id for file-level rows, so the symbol
    // cascade alone would orphan them — delete by file explicitly.
    tx.execute(
        "DELETE FROM contracts WHERE file = ?1",
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
        "DELETE FROM term_stats WHERE file = ?1",
        rusqlite::params![rel_path],
    )?;
    tx.execute(
        "DELETE FROM files WHERE path = ?1",
        rusqlite::params![rel_path],
    )?;

    // Incrementally repair the reach rows this deletion touched. On
    // failure, degrade: mark the table stale in this same transaction and
    // commit anyway — reach is a cache, and a stale table falls back to
    // BFS rather than serving wrong data (PRD-REACH-REQ-007).
    if let Err(e) = crate::reach::finish_file_edit(&tx, &scope) {
        crate::reach::mark_stale(&tx)?;
        eprintln!(
            "warn: incremental reach repair failed for {rel_path}: {e:#}; \
             table marked stale, queries fall back to BFS"
        );
    }

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

    // Capture the reach table's pre-edit view of this file BEFORE the old
    // rows are deleted (TASK-081, PRD-REACH-REQ-005): canonical ids and
    // reverse-target predecessors that only exist pre-delete.
    let scope = crate::reach::begin_file_edit(&tx, &result.rel_path)?;

    // Delete old type edges, symbols, references, and imports for this file.
    // type_edges has ON DELETE CASCADE from symbols, but we delete explicitly
    // for clarity and to mirror the pattern used for references and imports.
    tx.execute(
        "DELETE FROM type_edges WHERE child_id IN (SELECT id FROM symbols WHERE file = ?1)",
        rusqlite::params![result.rel_path],
    )?;
    // Contracts are cleared and rewritten per file on re-index; the symbol
    // cascade misses NULL-symbol_id rows (documents, top-level sites).
    tx.execute(
        "DELETE FROM contracts WHERE file = ?1",
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
    tx.execute(
        "DELETE FROM term_stats WHERE file = ?1",
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

    // Insert new references, resolving caller_name to caller_id and target_id.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO \"references\" (name, file, line, col, context, caller_id, confidence, target_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for reference in &result.refs {
            let caller_id = reference
                .caller_name
                .as_deref()
                .and_then(|name| caller_map.get(name).copied());
            // Same-file target resolution: if the referenced name is defined in this file, use its ID.
            let target_id = caller_map.get(reference.name.as_str()).copied();
            stmt.execute(rusqlite::params![
                reference.name,
                reference.file,
                reference.line as i64,
                reference.col as i64,
                reference.context,
                caller_id,
                reference.confidence,
                target_id,
            ])?;
        }
    }

    // Cross-file target_id resolution: for refs where the target wasn't in the same file,
    // resolve if there's exactly one symbol with that name.
    tx.execute(
        "UPDATE \"references\" SET target_id = ( \
             SELECT s.id FROM symbols s WHERE s.name = \"references\".name \
         ) WHERE file = ?1 AND target_id IS NULL \
         AND (SELECT COUNT(*) FROM symbols s WHERE s.name = \"references\".name) = 1",
        rusqlite::params![result.rel_path],
    )?;

    // Insert new imports.
    {
        let mut stmt =
            tx.prepare("INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)")?;
        for import in &result.imports {
            stmt.execute(rusqlite::params![result.rel_path, import])?;
        }
    }

    // Insert BM25 term statistics — same transaction as the file's symbols.
    let term_rows: Vec<(&str, &str, i64)> = result
        .term_freqs
        .iter()
        .map(|(term, tf)| (term.as_str(), result.rel_path.as_str(), *tf as i64))
        .collect();
    insert_term_stats(&tx, &term_rows)?;

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

    // Rewrite the file's contract rows (cleared above), resolving owning
    // symbols against the same name map the reference inserts used.
    insert_contracts(&tx, &result.rel_path, &result.contracts, Some(&caller_map))?;

    // Incrementally repair the reach rows this edit touched (the same
    // traversal the full build runs, over the affected source set). On
    // failure, degrade: mark the table stale in this same transaction and
    // commit anyway — reach is a cache, and a stale table falls back to
    // BFS rather than serving wrong data (PRD-REACH-REQ-007).
    if let Err(e) = crate::reach::finish_file_edit(&tx, &scope) {
        crate::reach::mark_stale(&tx)?;
        eprintln!(
            "warn: incremental reach repair failed for {}: {e:#}; \
             table marked stale, queries fall back to BFS",
            result.rel_path
        );
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
fn parse_one_file(
    path: &Path,
    repo_root: &Path,
    contract_opts: &crate::contracts::ContractOptions,
) -> Option<FileResult> {
    let Some(lang) = indexer::detect_language(path) else {
        // Not a grammar language: document files (.proto/.graphql/.yaml/
        // .json) may still carry contracts (TASK-088).
        return parse_document_file(path, repo_root, contract_opts);
    };
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

    // Extract contract candidates on the same tree (PRD-CTR-REQ-011).
    let contracts = crate::contracts::extract_contracts(&tree, &parse_source, lang, contract_opts);

    // Compute confidence for each reference.
    for r in &mut refs {
        r.confidence = indexer::compute_confidence(r, &symbols, &file_imports.imports);
    }

    let line_count = content.lines().count();
    let term_freqs = crate::tokenizer::term_frequencies(&content);

    Some(FileResult {
        rel_path,
        language: lang.name().to_string(),
        content_hash: hash,
        line_count,
        symbols,
        refs,
        imports: file_imports.imports,
        type_edges,
        term_freqs,
        contracts,
    })
}

/// Parse a document file (`.proto`/`.graphql`/`.yaml`/`.json`) for contracts
/// (TASK-088). Documents carry no symbols, references, imports, or type
/// edges; a file that yields no candidates returns `None` and stays
/// un-indexed exactly as before the document path existed.
fn parse_document_file(
    path: &Path,
    repo_root: &Path,
    contract_opts: &crate::contracts::ContractOptions,
) -> Option<FileResult> {
    // Lock files and oversized documents are rejected before the read: a
    // 100 MB package-lock.json must cost nothing on a full build.
    let kind = crate::contracts::scannable_document_kind(path)?;
    let content = std::fs::read_to_string(path).ok()?;
    let rel_path = path
        .strip_prefix(repo_root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    let content_hash = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(content.as_bytes()));
    document_file_result(kind, rel_path, &content, content_hash, contract_opts)
}

/// Build a document [`FileResult`] from already-read content.
///
/// `None` when the document yields no contracts (disabled kind, failed
/// OpenAPI sniff) — the caller leaves the file un-indexed. The row (language
/// set to the document kind, zero symbols) is the hash/re-index anchor for
/// document files: their contract rows are stored with a NULL symbol_id.
fn document_file_result(
    kind: crate::contracts::DocumentKind,
    rel_path: String,
    content: &str,
    content_hash: String,
    contract_opts: &crate::contracts::ContractOptions,
) -> Option<FileResult> {
    let contracts = crate::contracts::extract_document_contracts(kind, content, contract_opts);
    if contracts.is_empty() {
        return None;
    }
    Some(FileResult {
        rel_path,
        language: kind.as_str().to_string(),
        content_hash,
        line_count: content.lines().count(),
        symbols: Vec::new(),
        refs: Vec::new(),
        imports: Vec::new(),
        type_edges: Vec::new(),
        term_freqs: crate::tokenizer::term_frequencies(content),
        contracts,
    })
}

/// Insert all results into the database in a single transaction.
///
/// Returns (symbol_count, ref_count, caller_count, type_edge_count,
/// contract_count).
fn batch_insert(
    conn: &Connection,
    results: &[FileResult],
    reach_opts: Option<&crate::reach::ReachBuildOptions>,
) -> Result<(usize, usize, usize, usize, usize)> {
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

    // Insert references, resolving caller_name to caller_id and target_id.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO \"references\" (name, file, line, col, context, caller_id, confidence, target_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
                // Same-file target resolution.
                let target_id = file_map.and_then(|m| m.get(reference.name.as_str()).copied());
                stmt.execute(rusqlite::params![
                    reference.name,
                    reference.file,
                    reference.line as i64,
                    reference.col as i64,
                    reference.context,
                    caller_id,
                    reference.confidence,
                    target_id,
                ])?;
                total_refs += 1;
            }
        }
    }

    // Cross-file target_id resolution: for refs where the target wasn't in the same file,
    // resolve if there's exactly one symbol with that name.
    tx.execute(
        "UPDATE \"references\" SET target_id = ( \
             SELECT s.id FROM symbols s WHERE s.name = \"references\".name \
         ) WHERE target_id IS NULL \
         AND (SELECT COUNT(*) FROM symbols s WHERE s.name = \"references\".name) = 1",
        [],
    )?;

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

    // Insert BM25 term statistics — same transaction as the file's symbols.
    let mut term_rows: Vec<(&str, &str, i64)> = Vec::new();
    for r in results {
        for (term, tf) in &r.term_freqs {
            term_rows.push((term.as_str(), r.rel_path.as_str(), *tf as i64));
        }
    }
    insert_term_stats(&tx, &term_rows)?;

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

    // Insert contracts after symbols (FK on symbol_id is satisfied) and
    // before the reach build, in the same transaction.
    let mut contract_count = 0usize;
    for r in results {
        let file_map = file_caller_maps.get(r.rel_path.as_str());
        contract_count += insert_contracts(&tx, &r.rel_path, &r.contracts, file_map)?;
    }

    // Build the reach table in the same transaction as the symbols and
    // references it derives from, so readers never observe a partially
    // published reach set (PRD-REACH-REQ-008, AR-028).
    if let Some(opts) = reach_opts {
        crate::reach::build_reach(&tx, opts)?;
    }

    tx.commit().context("committing transaction")?;

    Ok((
        total_syms,
        total_refs,
        caller_count,
        type_edge_count,
        contract_count,
    ))
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
    provider: &dyn EmbeddingProvider,
    policy: EmbedErrorPolicy,
    silent: bool,
    replacement: EmbeddingReplacement<'_>,
    replacement_pending: &mut bool,
) -> Result<(usize, bool)> {
    let mut count = 0usize;
    for (sym_id, file, text) in batch {
        match provider.embed_single(text) {
            Ok(vec) => {
                if *replacement_pending {
                    replacement.prepare(conn)?;
                    *replacement_pending = false;
                }
                embedding::store_embeddings_batch(
                    conn,
                    provider,
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
/// batch via `provider`, and stores the resulting vectors. Returns the number
/// of successfully embedded symbols.
fn embed_chunks(
    conn: &Connection,
    chunks: &[(i64, String, String)],
    provider: &dyn EmbeddingProvider,
    progress_mode: ProgressMode,
    policy: EmbedErrorPolicy,
    replacement: EmbeddingReplacement<'_>,
) -> Result<usize> {
    let total = chunks.len();
    let silent = progress_mode == ProgressMode::Silent;
    let mut embedded = 0usize;
    let mut replacement_pending = true;

    for batch_start in (0..total).step_by(EMBEDDING_BATCH_SIZE) {
        let batch_end = (batch_start + EMBEDDING_BATCH_SIZE).min(total);
        let batch = &chunks[batch_start..batch_end];

        let texts: Vec<String> = batch.iter().map(|(_, _, text)| text.clone()).collect();

        let vectors = match provider.embed_batch(&texts) {
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
                let (fallback_count, should_break) = embed_batch_individually(
                    conn,
                    batch,
                    provider,
                    policy,
                    silent,
                    replacement,
                    &mut replacement_pending,
                )?;
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

        if replacement_pending {
            replacement.prepare(conn)?;
            replacement_pending = false;
        }
        embedding::store_embeddings_batch(conn, provider, &store_batch)
            .context("storing embedding batch")?;

        embedded += store_batch.len();

        render_embedding_progress(progress_mode, embedded, total);
    }

    // Clear the progress line if in-place mode.
    if progress_mode == ProgressMode::InPlace && embedded > 0 {
        eprintln!("\rEmbedded {embedded}/{total} symbols{:<40}", "");
    }

    Ok(embedded)
}

#[derive(Clone, Copy)]
enum EmbeddingReplacement<'a> {
    Incremental,
    All,
    Files(&'a [String]),
}

impl EmbeddingReplacement<'_> {
    fn prepare(self, conn: &Connection) -> Result<()> {
        match self {
            Self::Incremental => Ok(()),
            Self::All => {
                conn.execute("DELETE FROM embeddings", [])
                    .context("clearing old embeddings")?;
                Ok(())
            }
            Self::Files(files) => {
                let tx = conn
                    .unchecked_transaction()
                    .context("starting delete-embeddings transaction")?;
                for file in files {
                    embedding::delete_embeddings_for_file(&tx, file)
                        .context("deleting embeddings for changed file")?;
                }
                tx.commit()
                    .context("committing delete-embeddings transaction")
            }
        }
    }
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
    provider: &dyn EmbeddingProvider,
    progress_mode: ProgressMode,
) -> Result<EmbeddingBuildStats> {
    let start = Instant::now();

    // Health check.
    if !provider.is_healthy() {
        if progress_mode != ProgressMode::Silent {
            eprintln!(
                "embedding provider '{}' is unreachable — skipping embedding generation. \
                 Retry later, or re-embed with the bundled provider: \
                 `wonk update --force --provider bundled`",
                provider.name()
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

    let embedded = embed_chunks(
        conn,
        &chunks,
        provider,
        progress_mode,
        EmbedErrorPolicy::SkipPartial,
        EmbeddingReplacement::All,
    )?;

    Ok(EmbeddingBuildStats {
        embedded_count: embedded,
        total_symbols: total,
        skipped: embedded == 0,
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
    provider: &dyn EmbeddingProvider,
    progress_mode: ProgressMode,
) -> Result<EmbeddingBuildStats> {
    let start = Instant::now();

    // Generate chunks only for un-embedded / stale symbols.
    let chunks = embedding::chunk_missing_symbols(conn, repo_root, provider)
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
    // Unlike build_embeddings we return Err so the caller can decide how to
    // degrade (query-time fallback to the bundled provider).
    if !provider.is_healthy() {
        anyhow::bail!("{}", embedding::OLLAMA_UNREACHABLE_MSG);
    }

    let embedded = embed_chunks(
        conn,
        &chunks,
        provider,
        progress_mode,
        EmbedErrorPolicy::FailFast,
        EmbeddingReplacement::Incremental,
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
    provider: &dyn EmbeddingProvider,
) -> Result<usize> {
    if changed_files.is_empty() {
        return Ok(0);
    }

    if !provider.is_healthy() {
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

    // Generate chunks for the changed files.
    let chunks = embedding::chunk_symbols_for_files(conn, repo_root, changed_files)
        .context("chunking symbols for changed files")?;

    if chunks.is_empty() {
        EmbeddingReplacement::Files(changed_files).prepare(conn)?;
        return Ok(0);
    }

    // Embed and store (SkipPartial: daemon should not crash on embedding failures).
    let embedded = embed_chunks(
        conn,
        &chunks,
        provider,
        ProgressMode::Silent,
        EmbedErrorPolicy::SkipPartial,
        EmbeddingReplacement::Files(changed_files),
    )?;

    Ok(embedded)
}

/// Insert BM25 term statistics in the caller's transaction.
///
/// Rows are sorted by (term, file) and written via multi-row statements:
/// the primary-key B-tree receives sequential appends instead of random
/// inserts, and per-row execute overhead disappears on full builds.
fn insert_term_stats(tx: &rusqlite::Transaction, rows: &[(&str, &str, i64)]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut sorted = rows.to_vec();
    sorted.sort_unstable();

    // 3 bound parameters per row; bundled SQLite allows 32766 variables.
    const ROWS_PER_STMT: usize = 1000;
    for chunk in sorted.chunks(ROWS_PER_STMT) {
        let placeholders = chunk
            .iter()
            .map(|_| "(?, ?, ?)")
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("INSERT INTO term_stats (term, file, tf) VALUES {placeholders}");
        let mut stmt = tx.prepare(&sql)?;
        stmt.execute(rusqlite::params_from_iter(chunk.iter().flat_map(
            |(term, file, tf)| {
                [
                    term as &dyn rusqlite::ToSql,
                    file as &dyn rusqlite::ToSql,
                    tf as &dyn rusqlite::ToSql,
                ]
            },
        )))?;
    }
    Ok(())
}

/// Insert contract rows in the caller's transaction.
///
/// `symbol_id` resolves at write time against the SAME per-file name→id
/// map the reference path builds (last-wins semantics), so a contract and
/// the references in its owning function can never disagree about which
/// symbol owns a name. Document files (empty symbols) and unmatched or
/// missing owning symbols store NULL — the row keeps its file/line anchors.
/// Returns the number of rows inserted; UNIQUE(canonical_id, role, file,
/// line) collisions are ignored, mirroring type_edges.
fn insert_contracts(
    tx: &rusqlite::Transaction,
    rel_path: &str,
    contracts: &[ContractCandidate],
    name_map: Option<&HashMap<&str, i64>>,
) -> Result<usize> {
    let mut inserted = 0usize;
    let mut stmt = tx.prepare(
        "INSERT OR IGNORE INTO contracts (canonical_id, kind, role, symbol_id, file, line, confidence) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for c in contracts {
        let symbol_id = c
            .owning_symbol
            .as_deref()
            .and_then(|name| name_map?.get(name).copied());
        inserted += stmt.execute(rusqlite::params![
            c.canonical_id,
            c.kind.as_str(),
            c.role.as_str(),
            symbol_id,
            rel_path,
            c.line as i64,
            c.confidence,
        ])?;
    }
    Ok(inserted)
}

/// Drop all data from the main tables (used before rebuild).
fn drop_all_data(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM embeddings;
         DELETE FROM type_edges;
         DELETE FROM contracts;
         DELETE FROM symbols;
         DELETE FROM \"references\";
         DELETE FROM file_imports;
         DELETE FROM term_stats;
         DELETE FROM reach;
         DELETE FROM reach_truncated;
         DELETE FROM reach_meta;
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

    struct TwoDimProvider;
    struct FailingProvider;
    struct OldTwoDimProvider;

    impl embedding::EmbeddingProvider for TwoDimProvider {
        fn name(&self) -> &str {
            "test"
        }

        fn dim(&self) -> usize {
            2
        }

        fn embed_batch(&self, chunks: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Ok(chunks.iter().map(|_| vec![1.0, 0.0]).collect())
        }
    }

    impl embedding::EmbeddingProvider for FailingProvider {
        fn name(&self) -> &str {
            "failing"
        }

        fn dim(&self) -> usize {
            2
        }

        fn embed_batch(&self, _chunks: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Err(EmbeddingError::OllamaUnreachable)
        }
    }

    impl embedding::EmbeddingProvider for OldTwoDimProvider {
        fn name(&self) -> &str {
            "old-test"
        }

        fn dim(&self) -> usize {
            2
        }

        fn embed_batch(&self, chunks: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Ok(chunks.iter().map(|_| vec![0.0, 1.0]).collect())
        }
    }

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

    fn make_contract_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/app.js"),
            "const app = express();\n             app.get('/v1/users/:id', getUser);\n             app.post('/orders', createOrder);\n             const db = process.env.DATABASE_URL;\n",
        )
        .unwrap();
        fs::write(root.join("src/util.txt"), "not code\n").unwrap();
        dir
    }

    #[test]
    fn build_index_reports_contract_count() {
        let dir = make_contract_repo();
        let stats = build_index(dir.path(), true).unwrap();
        // 2 HTTP providers + 1 env consumer; util.txt contributes nothing.
        assert_eq!(stats.contract_count, 3, "got {stats:?}");
    }

    #[test]
    fn test_build_index_contracts_kind_disabled_by_config() {
        let dir = make_contract_repo();
        write_reach_config(dir.path(), "[contracts]\nhttp = false\n");
        let stats = build_index(dir.path(), true).unwrap();
        // Only the env read survives when http detection is off.
        assert_eq!(stats.contract_count, 1, "got {stats:?}");
    }

    #[test]
    fn test_build_index_contracts_all_disabled_by_config() {
        let dir = make_contract_repo();
        write_reach_config(
            dir.path(),
            "[contracts]\nhttp = false\nenv = false\nqueue = false\nwebsocket = false\njob = false\ngrpc = false\ngraphql = false\nopenapi = false\n",
        );
        let stats = build_index(dir.path(), true).unwrap();
        assert_eq!(stats.contract_count, 0, "got {stats:?}");
    }

    #[test]
    fn build_index_persists_contracts() {
        let dir = make_contract_repo();
        let stats = build_index(dir.path(), true).unwrap();
        assert_eq!(stats.contract_count, 3, "got {stats:?}");

        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM contracts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3, "every detected candidate must be stored");

        let (kind, role, file, line, confidence): (String, String, String, i64, f64) = conn
            .query_row(
                "SELECT kind, role, file, line, confidence FROM contracts \
                 WHERE canonical_id = 'env::::DATABASE_URL'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .expect("env consumer row must exist");
        assert_eq!(kind, "env");
        assert_eq!(role, "consumer");
        assert_eq!(file, "src/app.js");
        assert_eq!(line, 4);
        assert_eq!(confidence, 1.0);
    }

    #[test]
    fn build_index_resolves_symbol_id() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        // A route registered inside a named function, a fetch consumer
        // inside another function, and a top-level registration.
        fs::write(
            root.join("src/routes.js"),
            "const app = express();\n\nfunction registerRoutes() {\n  app.get('/v1/users/:id', getUser);\n}\n\nasync function load() {\n  const r = await fetch('https://api.io/v1/users');\n}\n\napp.post('/orders', createOrder);\n",
        )
        .unwrap();

        build_index(root, true).unwrap();
        let conn = db::open_existing(&db::local_index_path(root)).unwrap();

        let symbol_id_for = |name: &str| -> i64 {
            conn.query_row(
                "SELECT id FROM symbols WHERE name = ?1 AND file = 'src/routes.js'",
                rusqlite::params![name],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("symbol {name} must be indexed: {e}"))
        };
        let contract_symbol_id = |canonical: &str| -> Option<i64> {
            conn.query_row(
                "SELECT symbol_id FROM contracts WHERE canonical_id = ?1",
                rusqlite::params![canonical],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("contract {canonical} must be stored: {e}"))
        };

        assert_eq!(
            contract_symbol_id("http::GET::/v1/users/{p1}"),
            Some(symbol_id_for("registerRoutes")),
            "route registered inside registerRoutes() resolves to it"
        );
        assert_eq!(
            contract_symbol_id("http::GET::/v1/users"),
            Some(symbol_id_for("load")),
            "fetch consumer inside load() resolves to it"
        );
        assert_eq!(
            contract_symbol_id("http::POST::/orders"),
            None,
            "top-level registration has no owning symbol -> NULL"
        );
    }

    #[test]
    fn document_contracts_persist_with_null_symbol_id() {
        let dir = make_rpc_contract_repo();
        let stats = build_index(dir.path(), true).unwrap();
        assert_eq!(stats.contract_count, 2, "got {stats:?}");

        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();
        let rows: Vec<(String, String, Option<i64>)> = conn
            .prepare("SELECT canonical_id, role, symbol_id FROM contracts ORDER BY canonical_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|(_, _, id)| id.is_none()));
        assert_eq!(rows[0].0, "grpc::UserService::GetUser");
        assert_eq!(rows[0].1, "provider");
    }

    // -- document files (TASK-088, DQ1) ----------------------------------------

    /// Repo with a contract-bearing proto document plus files that must stay
    /// un-indexed: a non-document text file, a non-sniffing YAML, and a
    /// package.json.
    fn make_rpc_contract_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("proto")).unwrap();
        fs::write(
            root.join("proto/users.proto"),
            "syntax = \"proto3\";\npackage users.v1;\n\nservice UserService {\n  rpc GetUser(GetUserRequest) returns (User);\n  rpc ListUsers(ListUsersRequest) returns (stream User);\n}\n",
        )
        .unwrap();
        fs::write(root.join("notes.txt"), "notes are not documents\n").unwrap();
        fs::write(
            root.join("docker-compose.yml"),
            "services:\n  app:\n    image: busybox\n",
        )
        .unwrap();
        fs::write(root.join("package.json"), "{\n  \"name\": \"x\"\n}\n").unwrap();
        dir
    }

    #[test]
    fn build_index_indexes_proto_document_file() {
        let dir = make_rpc_contract_repo();
        let stats = build_index(dir.path(), true).unwrap();
        // Two rpc methods -> two grpc providers; the other files contribute
        // nothing.
        assert_eq!(stats.contract_count, 2, "got {stats:?}");

        let index_path = db::local_index_path(dir.path());
        let conn = db::open_existing(&index_path).unwrap();
        let (language, symbols_count): (String, i64) = conn
            .query_row(
                "SELECT language, symbols_count FROM files WHERE path = 'proto/users.proto'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("proto file must have a files row");
        assert_eq!(language, "Proto");
        assert_eq!(symbols_count, 0);

        // meta.json carries the document language.
        let meta = db::read_meta(&index_path).unwrap();
        assert!(
            meta.languages.iter().any(|l| l == "Proto"),
            "got {:?}",
            meta.languages
        );
    }

    #[test]
    fn build_index_skips_non_document_and_non_sniffing_files() {
        let dir = make_rpc_contract_repo();
        build_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();
        let skipped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path IN ('notes.txt', 'docker-compose.yml', 'package.json')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(skipped, 0);
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1, "only the proto document is indexed");
    }

    #[test]
    fn build_index_skips_lockfile_documents() {
        // A repo-root lock file matches a document extension (here even
        // sniffs as OpenAPI): lock/data files never carry contracts, so the
        // document path must skip them by name before reading anything.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(
            root.join("package-lock.json"),
            "{\n  \"openapi\": \"3.0.0\",\n  \"paths\": {\n    \"/x\": {\n      \"get\": {\n        \"description\": \"sniffs positive\"\n      }\n    }\n  }\n}\n",
        )
        .unwrap();
        let stats = build_index(root, true).unwrap();
        assert_eq!(stats.file_count, 0, "got {stats:?}");
        assert_eq!(stats.contract_count, 0, "got {stats:?}");
    }

    #[test]
    fn build_index_skips_oversized_documents() {
        // A document above MAX_DOCUMENT_SCAN_BYTES stays un-indexed exactly
        // as before the document path existed: scanning tens of MB of data
        // YAML can only produce a guaranteed-null sniff result.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        let mut spec = String::from("openapi: 3.0.0\npaths:\n");
        while spec.len() <= crate::contracts::MAX_DOCUMENT_SCAN_BYTES as usize {
            spec.push_str("  /pad/x:\n    get:\n      summary: pad\n");
        }
        fs::write(root.join("openapi.yaml"), spec).unwrap();
        let stats = build_index(root, true).unwrap();
        assert_eq!(stats.file_count, 0, "got {stats:?}");
        assert_eq!(stats.contract_count, 0, "got {stats:?}");
    }

    #[test]
    fn reindex_file_skips_lockfile_document() {
        // The incremental path pays the same guard: a changed lock file
        // re-hashes but never gains a row.
        let dir = make_rpc_contract_repo();
        build_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();
        let lock = dir.path().join("package-lock.json");
        fs::write(
            &lock,
            "{\n  \"openapi\": \"3.0.0\",\n  \"paths\": {\n    \"/x\": {\n      \"get\": {}\n    }\n  }\n}\n",
        )
        .unwrap();
        let opts = crate::contracts::ContractOptions::default();
        assert!(!reindex_file(&conn, &lock, dir.path(), &opts).unwrap());
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'package-lock.json'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "lock files must stay un-indexed");
    }

    #[test]
    fn reindex_file_document_hash_skip_and_reindex() {
        let dir = make_rpc_contract_repo();
        build_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();
        let proto = dir.path().join("proto/users.proto");
        let opts = crate::contracts::ContractOptions::default();

        // Unchanged document: hash match skips the reindex.
        assert!(!reindex_file(&conn, &proto, dir.path(), &opts).unwrap());

        // Add a method: the document re-indexes and the new hash lands.
        let updated = "syntax = \"proto3\";\npackage users.v1;\n\nservice UserService {\n  rpc GetUser(GetUserRequest) returns (User);\n  rpc ListUsers(ListUsersRequest) returns (stream User);\n  rpc DeleteUser(DeleteUserRequest) returns (Empty);\n}\n";
        fs::write(&proto, updated).unwrap();
        assert!(reindex_file(&conn, &proto, dir.path(), &opts).unwrap());
        let hash: String = conn
            .query_row(
                "SELECT hash FROM files WHERE path = 'proto/users.proto'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let expected = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(updated.as_bytes()));
        assert_eq!(hash, expected);
    }

    #[test]
    fn reindex_file_stale_document_row_removed_when_contracts_vanish() {
        // A document edited down to zero contracts must not keep a stale
        // files row: reindex deletes the row and reports "not re-indexed".
        let dir = make_rpc_contract_repo();
        build_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();
        let proto = dir.path().join("proto/users.proto");
        fs::write(
            &proto,
            "syntax = \"proto3\";\npackage users.v1;\n\nmessage User { string id = 1; }\n",
        )
        .unwrap();
        let opts = crate::contracts::ContractOptions::default();
        assert!(
            !reindex_file(&conn, &proto, dir.path(), &opts).unwrap(),
            "no contracts left -> no re-index, row cleaned"
        );
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'proto/users.proto'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "contract-less document must be un-indexed");
    }

    #[test]
    fn remove_file_cleans_document_row() {
        let dir = make_rpc_contract_repo();
        build_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();
        remove_file(&conn, &dir.path().join("proto/users.proto"), dir.path()).unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'proto/users.proto'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn reindex_replaces_file_contracts() {
        let dir = make_contract_repo();
        build_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();
        let file = dir.path().join("src/app.js");

        fs::write(
            &file,
            "const app = express();\napp.get('/v2/ping', ping);\nconst flag = process.env.FEATURE_X;\n",
        )
        .unwrap();
        assert!(
            reindex_file(
                &conn,
                &file,
                dir.path(),
                &crate::contracts::ContractOptions::default()
            )
            .unwrap()
        );

        let ids: Vec<String> = conn
            .prepare("SELECT canonical_id FROM contracts WHERE file = 'src/app.js' ORDER BY canonical_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            ids,
            vec![
                "env::::FEATURE_X".to_string(),
                "http::GET::/v2/ping".to_string()
            ],
            "re-index must leave exactly the new-content set, no stale rows"
        );
    }

    #[test]
    fn remove_file_deletes_contracts() {
        let dir = make_contract_repo();
        build_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();
        remove_file(&conn, &dir.path().join("src/app.js"), dir.path()).unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM contracts WHERE file = 'src/app.js'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "deleting a file must drop its contract rows");
    }

    #[test]
    fn reindex_file_stale_document_contracts_removed() {
        // A proto edited from two rpc methods down to one must not keep the
        // vanished method's row: the document flows through the same
        // per-file delete-and-rewrite seam as grammar files.
        let dir = make_rpc_contract_repo();
        build_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();
        let proto = dir.path().join("proto/users.proto");
        fs::write(
            &proto,
            "syntax = \"proto3\";\npackage users.v1;\n\nservice UserService {\n  rpc GetUser(GetUserRequest) returns (User);\n}\n",
        )
        .unwrap();
        assert!(
            reindex_file(
                &conn,
                &proto,
                dir.path(),
                &crate::contracts::ContractOptions::default()
            )
            .unwrap()
        );
        let ids: Vec<String> = conn
            .prepare("SELECT canonical_id FROM contracts WHERE file = 'proto/users.proto'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec!["grpc::UserService::GetUser".to_string()]);
    }

    #[test]
    fn rebuild_index_clears_contracts() {
        let dir = make_contract_repo();
        build_index(dir.path(), true).unwrap();
        let stats = rebuild_index(dir.path(), true).unwrap();

        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM contracts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count as usize, stats.contract_count);
        assert_eq!(stats.contract_count, 3, "rebuild must not duplicate rows");
    }

    #[test]
    fn reindex_file_extracts_contracts() {
        // reindex_file runs the contract extractor inline and skips
        // unchanged content by hash.
        let dir = make_contract_repo();
        let stats = build_index(dir.path(), true).unwrap();
        assert_eq!(stats.contract_count, 3);

        let index_path = db::local_index_path(dir.path());
        let conn = db::open_existing(&index_path).unwrap();
        let file = dir.path().join("src/app.js");
        // Unchanged content: hash match skips the reindex.
        assert!(
            !reindex_file(
                &conn,
                &file,
                dir.path(),
                &crate::contracts::ContractOptions::default()
            )
            .unwrap()
        );

        fs::write(
            &file,
            "const app = express();\n             app.get('/v1/users/:id', getUser);\n             app.post('/orders', createOrder);\n             app.delete('/orders/:id', deleteOrder);\n             const db = process.env.DATABASE_URL;\n",
        )
        .unwrap();
        assert!(
            reindex_file(
                &conn,
                &file,
                dir.path(),
                &crate::contracts::ContractOptions::default()
            )
            .unwrap()
        );
        assert!(
            !reindex_file(
                &conn,
                &file,
                dir.path(),
                &crate::contracts::ContractOptions::default()
            )
            .unwrap()
        );
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

    // -----------------------------------------------------------------------
    // term_stats (TASK-078)
    // -----------------------------------------------------------------------

    /// Assert the DB term_stats are exactly what the tokenizer oracle
    /// produces from the current disk content of every indexed file, with
    /// no orphan rows and no missing document lengths.
    fn assert_stats_match_disk(conn: &Connection, root: &Path) {
        let paths: Vec<String> = conn
            .prepare("SELECT path FROM files")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(!paths.is_empty(), "index should contain files");

        for rel in &paths {
            let content =
                fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"));
            let expected = crate::tokenizer::term_frequencies(&content);
            let actual: HashMap<String, i64> = conn
                .prepare("SELECT term, tf FROM term_stats WHERE file = ?1")
                .unwrap()
                .query_map(rusqlite::params![rel], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            assert_eq!(
                actual.len(),
                expected.len(),
                "{rel}: distinct term count differs from tokenizer oracle"
            );
            for (term, tf) in &expected {
                assert_eq!(
                    actual.get(term).copied(),
                    Some(*tf as i64),
                    "{rel}: tf for term '{term}' differs from tokenizer oracle"
                );
            }
        }

        // No orphan rows: every stats row must reference an indexed file.
        let orphans: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM term_stats \
                 WHERE file NOT IN (SELECT path FROM files)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0, "term_stats rows must not outlive their file");

        // Document lengths are BM25's |D| — never NULL for files with stats.
        let null_lengths: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE line_count IS NULL \
                 AND path IN (SELECT DISTINCT file FROM term_stats)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(null_lengths, 0, "files with stats must have a line_count");
    }

    #[test]
    fn test_build_index_populates_term_stats() {
        let dir = make_test_repo();
        let _stats = build_index(dir.path(), true).unwrap();

        let index_path = db::local_index_path(dir.path());
        let conn = db::open_existing(&index_path).unwrap();

        // src/main.rs contains "helper" twice: the call site and the
        // definition. Lowercased alphanumeric tokens, punctuation stripped.
        let tf_helper: i64 = conn
            .query_row(
                "SELECT tf FROM term_stats WHERE term = 'helper' AND file = 'src/main.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tf_helper, 2, "tf for 'helper' in src/main.rs");

        // "helper" only occurs in src/main.rs → document frequency 1.
        let df_helper: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM term_stats WHERE term = 'helper'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(df_helper, 1);

        // Every indexed file contributes at least one distinct term.
        let total_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM term_stats", [], |row| row.get(0))
            .unwrap();
        let file_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .unwrap();
        assert!(
            total_rows >= file_count,
            "each indexed file should have term_stats rows ({total_rows} rows for {file_count} files)"
        );
    }

    #[test]
    fn test_term_stats_tf_matches_tokenizer_per_file() {
        let dir = make_test_repo();
        let _stats = build_index(dir.path(), true).unwrap();

        let index_path = db::local_index_path(dir.path());
        let conn = db::open_existing(&index_path).unwrap();

        assert_stats_match_disk(&conn, dir.path());
    }

    #[test]
    fn test_reindex_file_updates_term_stats() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Rewrite lib.rs with different term content.
        fs::write(
            root.join("lib.rs"),
            "fn goodbye() { 100 }\nfn farewell() { 200 }",
        )
        .unwrap();
        let changed = reindex_file(
            &conn,
            &root.join("lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        assert!(changed, "modified file should be re-indexed");

        // Old terms are gone, new terms carry correct tf.
        let stale: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM term_stats WHERE file = 'lib.rs' AND term = 'hello'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0, "stats for removed terms must be deleted");

        let tf_goodbye: i64 = conn
            .query_row(
                "SELECT tf FROM term_stats WHERE file = 'lib.rs' AND term = 'goodbye'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tf_goodbye, 1);

        assert_stats_match_disk(&conn, root);
    }

    #[test]
    fn test_reindex_file_unchanged_leaves_stats() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        let rows_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM term_stats", [], |row| row.get(0))
            .unwrap();

        let changed = reindex_file(
            &conn,
            &root.join("lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        assert!(!changed, "unchanged file should be skipped");

        let rows_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM term_stats", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            rows_before, rows_after,
            "unchanged-hash early exit must leave term_stats untouched"
        );
        assert_stats_match_disk(&conn, root);
    }

    #[test]
    fn test_remove_file_deletes_term_stats() {
        let (dir, conn) = setup_indexed_repo();

        // "hello" occurs only in lib.rs.
        let df_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM term_stats WHERE term = 'hello'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(df_before, 1, "'hello' should start in exactly one file");

        remove_file(&conn, &dir.path().join("lib.rs"), dir.path()).unwrap();

        // Document frequency decrements exactly — no orphaned postings.
        let df_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM term_stats WHERE term = 'hello'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(df_after, 0, "df must drop to zero when its only file goes");

        let lib_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM term_stats WHERE file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lib_rows, 0);
    }

    #[test]
    fn incremental_update_reports_stored_contract_count() {
        let dir = make_contract_repo();
        build_index(dir.path(), true).unwrap();

        // Change app.js: one route replaced, one added -> four stored rows.
        let file = dir.path().join("src/app.js");
        fs::write(
            &file,
            "const app = express();\napp.get('/v1/users/:id', getUser);\napp.post('/orders', createOrder);\napp.delete('/orders/:id', deleteOrder);\nconst db = process.env.DATABASE_URL;\n",
        )
        .unwrap();
        let stats = incremental_update(dir.path(), true).unwrap();
        assert_eq!(stats.contract_count, 4, "got {stats:?}");

        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM contracts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 4, "stats must reflect what is in the database");
    }

    #[test]
    fn test_incremental_update_removes_term_stats() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();
        drop(conn);

        // Delete a file from disk, then run the incremental update pass.
        fs::remove_file(root.join("lib.rs")).unwrap();
        let _stats = incremental_update(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        let lib_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM term_stats WHERE file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            lib_rows, 0,
            "incremental update must purge stats of deleted files"
        );

        assert_stats_match_disk(&conn, root);
    }

    #[test]
    fn test_rename_as_delete_and_insert_no_orphans() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        // Rename lib.rs -> renamed.rs on disk, then feed the pipeline the
        // events the watcher derives from a rename: Deleted(old) plus
        // Created(new) (the old path no longer exists, the new one does).
        fs::rename(root.join("lib.rs"), root.join("renamed.rs")).unwrap();
        let events = vec![
            FileEvent::Deleted(root.join("lib.rs")),
            FileEvent::Created(root.join("renamed.rs")),
        ];
        let result = process_events(
            &conn,
            &events,
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        assert_eq!(result.updated_count, 2, "both rename halves get processed");

        let old_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM term_stats WHERE file = 'lib.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_rows, 0, "stats must not linger under the old name");

        let new_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM term_stats WHERE file = 'renamed.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(new_rows > 0, "stats must exist under the new name");

        assert_stats_match_disk(&conn, root);
    }

    #[test]
    fn test_edit_delete_rename_sequence_keeps_stats_consistent() {
        let dir = make_test_repo();
        let root = dir.path();
        build_index(root, true).unwrap();
        let conn = db::open_existing(&db::local_index_path(root)).unwrap();
        assert_stats_match_disk(&conn, root);

        // Edit an indexed file.
        fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    let v = reindex_me();\n    v\n}\n",
        )
        .unwrap();
        reindex_file(
            &conn,
            &root.join("src/main.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        assert_stats_match_disk(&conn, root);

        // Create a new file.
        fs::write(
            root.join("newmod.rs"),
            "fn fresh() {\n    alpha beta alpha\n}\n",
        )
        .unwrap();
        index_new_file(
            &conn,
            &root.join("newmod.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        assert_stats_match_disk(&conn, root);

        // Rename it: delete + insert events.
        fs::rename(root.join("newmod.rs"), root.join("moved.rs")).unwrap();
        let events = vec![
            FileEvent::Deleted(root.join("newmod.rs")),
            FileEvent::Created(root.join("moved.rs")),
        ];
        process_events(
            &conn,
            &events,
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        assert_stats_match_disk(&conn, root);

        // Delete a file.
        fs::remove_file(root.join("app.py")).unwrap();
        process_events(
            &conn,
            &[FileEvent::Deleted(root.join("app.py"))],
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        assert_stats_match_disk(&conn, root);

        // Edit the renamed file again.
        fs::write(root.join("moved.rs"), "fn fresh() {\n    gamma delta\n}\n").unwrap();
        reindex_file(
            &conn,
            &root.join("moved.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        assert_stats_match_disk(&conn, root);
    }

    #[test]
    fn test_drop_all_data_clears_term_stats() {
        // The TempDir must outlive the test; only the connection is used.
        let (_dir, conn) = setup_indexed_repo();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM term_stats", [], |row| row.get(0))
            .unwrap();
        assert!(before > 0, "index should carry term stats before the drop");

        drop_all_data(&conn).unwrap();

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM term_stats", [], |row| row.get(0))
            .unwrap();
        assert_eq!(after, 0, "drop_all_data must clear term_stats");
    }

    // -- Reach pipeline integration (TASK-080) -------------------------------

    fn reach_meta_value(conn: &Connection, key: &str) -> Option<String> {
        conn.query_row(
            "SELECT value FROM reach_meta WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get(0),
        )
        .ok()
    }

    fn reach_row_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM reach", [], |row| row.get(0))
            .unwrap()
    }

    /// Write a per-repo `[reach]` config before indexing.
    fn write_reach_config(root: &std::path::Path, toml: &str) {
        fs::create_dir_all(root.join(".wonk")).unwrap();
        fs::write(root.join(".wonk/config.toml"), toml).unwrap();
    }

    #[test]
    fn test_build_index_populates_reach() {
        let dir = make_test_repo();
        build_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();

        assert_eq!(
            reach_meta_value(&conn, "built_depth").as_deref(),
            Some("3"),
            "default build depth is 3"
        );
        assert!(
            reach_meta_value(&conn, "stale").is_none(),
            "fresh build is not stale"
        );
        assert!(
            reach_row_count(&conn) > 0,
            "the test repo's call graph should yield reach rows"
        );
    }

    #[test]
    fn test_build_index_reach_disabled_skips_table() {
        let dir = make_test_repo();
        write_reach_config(dir.path(), "[reach]\nenabled = false\n");
        build_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();

        assert!(
            reach_meta_value(&conn, "built_depth").is_none(),
            "disabled reach must not populate the table"
        );
        assert_eq!(reach_row_count(&conn), 0);
    }

    #[test]
    fn test_build_index_reach_depth_config_recorded() {
        let dir = make_test_repo();
        write_reach_config(dir.path(), "[reach]\ndepth = 2\n");
        build_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();

        assert_eq!(reach_meta_value(&conn, "built_depth").as_deref(), Some("2"));
        // A depth-3 query is beyond the built depth: no authoritative answer.
        assert!(
            crate::reach::lookup_upstream(&conn, "helper", 3)
                .unwrap()
                .is_none()
        );
        // Depth within the built depth answers.
        assert!(
            crate::reach::lookup_upstream(&conn, "helper", 2)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_rebuild_replaces_reach_rows() {
        let dir = make_test_repo();
        build_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();

        // Add a caller and rebuild: the new edge must appear, in one build.
        drop(conn);
        fs::write(
            dir.path().join("src/main.rs"),
            r#"fn main() {
    let x = helper();
    let y = extra();
    println!("{}{}", x, y);
}

fn helper() -> i32 {
    42
}

fn extra() -> i32 {
    7
}
"#,
        )
        .unwrap();
        rebuild_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&db::local_index_path(dir.path())).unwrap();

        let answer = crate::reach::lookup_upstream(&conn, "helper", 3)
            .unwrap()
            .expect("table covers the rebuilt index");
        assert!(
            answer.affected.iter().any(|s| s.name == "main"),
            "main calls helper"
        );
        assert!(
            reach_meta_value(&conn, "stale").is_none(),
            "full rebuild clears staleness"
        );
    }

    #[test]
    fn test_reindex_file_repairs_reach() {
        let (dir, conn) = setup_indexed_repo();
        assert!(reach_meta_value(&conn, "stale").is_none());

        // Touch a file with new content: the daemon's incremental path.
        fs::write(
            dir.path().join("lib.rs"),
            "fn hello() { world(); }\nfn world() { 4 }",
        )
        .unwrap();
        reindex_file(
            &conn,
            &dir.path().join("lib.rs"),
            dir.path(),
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();

        assert!(
            reach_meta_value(&conn, "stale").is_none(),
            "reindex repairs the table instead of marking it stale (REQ-005)"
        );
        // The repaired table answers, and equals the BFS.
        crate::reach::assert_table_equivalent_to_bfs(&conn);
        let answer = crate::reach::lookup_upstream(&conn, "world", 3)
            .unwrap()
            .expect("repaired table covers world");
        assert!(
            answer.affected.iter().any(|s| s.name == "hello"),
            "hello calls world"
        );
    }

    #[test]
    fn test_remove_file_repairs_reach() {
        let (dir, conn) = setup_indexed_repo();
        assert!(reach_meta_value(&conn, "stale").is_none());

        remove_file(&conn, &dir.path().join("app.py"), dir.path()).unwrap();

        assert!(
            reach_meta_value(&conn, "stale").is_none(),
            "file removal repairs the table instead of marking it stale"
        );
        crate::reach::assert_table_equivalent_to_bfs(&conn);
    }

    #[test]
    fn test_stale_reach_still_answers_via_blast() {
        let (dir, conn) = setup_indexed_repo();

        // Mark stale directly: this is now a degrade-path test, not the
        // normal reindex behavior (which repairs).
        fs::write(
            dir.path().join("lib.rs"),
            "fn hello() { world(); }\nfn world() { 4 }",
        )
        .unwrap();
        reindex_file(
            &conn,
            &dir.path().join("lib.rs"),
            dir.path(),
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        crate::reach::mark_stale(&tx).unwrap();
        tx.commit().unwrap();
        assert!(reach_meta_value(&conn, "stale").is_some());

        // analyze_blast must silently degrade to BFS, not error or go empty.
        let options = crate::blast::BlastOptions::default();
        let result = crate::blast::analyze_blast(&conn, "world", &options).unwrap();
        let names: Vec<&str> = result
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert!(
            names.contains(&"hello"),
            "stale table degrades to BFS and still finds callers"
        );
    }

    /// REQ-007: a failed incremental repair must degrade to BFS — the file
    /// data still commits, the table is marked stale in the same
    /// transaction, lookups return None, and blast answers equal the plain
    /// BFS. The failpoint self-clears after one shot. The probe file is
    /// uniquely named so no parallel test's reindex can consume the
    /// path-keyed injection.
    #[test]
    fn test_reindex_repair_failure_degrades_to_bfs_never_wrong_data() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(
            root.join("degrade_probe.rs"),
            "fn hello() { world(); }\nfn world() { 2 }",
        )
        .unwrap();
        build_index(root, true).unwrap();
        let conn = db::open_existing(&db::local_index_path(root)).unwrap();

        // Sanity: the table covers world before the edit.
        assert!(
            crate::reach::lookup_upstream(&conn, "world", 3)
                .unwrap()
                .is_some()
        );

        *crate::reach::FAIL_NEXT_FINISH.lock().unwrap() = Some("degrade_probe.rs".to_string());
        fs::write(
            root.join("degrade_probe.rs"),
            "fn hello() { world(); }\nfn world() { 4 }\nfn extra() { 7 }",
        )
        .unwrap();
        let changed = reindex_file(
            &conn,
            &root.join("degrade_probe.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();

        assert!(changed, "the reindex itself succeeds");
        // The file data committed despite the failed repair.
        let symbols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'degrade_probe.rs' AND name = 'extra'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(symbols, 1, "file data commits even when the repair fails");
        // The table was marked stale in the same transaction.
        assert_eq!(
            reach_meta_value(&conn, "stale").as_deref(),
            Some("1"),
            "failed repair degrades by marking stale (REQ-007)"
        );
        assert!(
            crate::reach::lookup_upstream(&conn, "world", 3)
                .unwrap()
                .is_none(),
            "stale table must not answer"
        );

        // The default (use_reach) path equals the plain BFS: never wrong.
        let via_table =
            crate::blast::analyze_blast(&conn, "world", &crate::blast::BlastOptions::default())
                .unwrap();
        let via_bfs = crate::blast::analyze_blast(
            &conn,
            "world",
            &crate::blast::BlastOptions {
                use_reach: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(via_table, via_bfs);
        let names: Vec<&str> = via_table
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert!(names.contains(&"hello"), "BFS still finds the caller");

        // One-shot: the failpoint cleared itself.
        assert!(crate::reach::FAIL_NEXT_FINISH.lock().unwrap().is_none());
    }

    /// A Rust file of `g_fns` functions that each call `world` (defined
    /// in the same file). Editing it yields a reach rebuild set of exactly
    /// `g_fns + 1` names — the dial for the work-budget boundary tests.
    fn wide_rust_source(g_fns: usize) -> String {
        let mut src = String::from("fn world() -> u32 {\n    1\n}\n");
        for i in 0..g_fns {
            src.push_str(&format!("fn g{i}() -> u32 {{\n    world()\n}}\n"));
        }
        src
    }

    /// Work-budget guard, natural trip (TASK-081, PRD-DMN-REQ-009): when
    /// an edit's rebuild set exceeds `MAX_INCREMENTAL_REPAIR_SOURCES`,
    /// the refused repair flows through the same degrade wiring as the
    /// injected failure — reindex succeeds, file data commits, the table
    /// is marked stale in the same transaction, lookups fall back to BFS,
    /// and the default blast path equals the plain BFS. No failpoint is
    /// set: the graph itself is over the budget.
    #[test]
    fn test_reindex_oversized_repair_degrades_to_bfs_via_work_budget() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        // g-fns + world => a rebuild set of g_fns + 1 names. g_fns = MAX
        // puts the set at MAX + 1: over the budget by exactly one source.
        fs::write(
            root.join("wide.rs"),
            wide_rust_source(crate::reach::MAX_INCREMENTAL_REPAIR_SOURCES),
        )
        .unwrap();
        build_index(root, true).unwrap();
        let conn = db::open_existing(&db::local_index_path(root)).unwrap();

        // Sanity: the table covers world before the edit.
        assert!(
            crate::reach::lookup_upstream(&conn, "world", 3)
                .unwrap()
                .is_some()
        );

        let wide = root.join("wide.rs");
        let base = fs::read_to_string(&wide).unwrap();
        fs::write(&wide, format!("{base}// budget edit\n")).unwrap();
        let changed = reindex_file(
            &conn,
            &wide,
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();

        assert!(changed, "the reindex itself succeeds");
        // The file data committed despite the refused repair.
        let symbols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file = 'wide.rs' AND name LIKE 'g%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            symbols as usize,
            crate::reach::MAX_INCREMENTAL_REPAIR_SOURCES,
            "file data commits even when the repair is refused"
        );
        // The table was marked stale in the same transaction.
        assert_eq!(
            reach_meta_value(&conn, "stale").as_deref(),
            Some("1"),
            "over-budget repair degrades by marking stale (REQ-007)"
        );
        assert!(
            crate::reach::lookup_upstream(&conn, "world", 3)
                .unwrap()
                .is_none(),
            "stale table must not answer"
        );

        // The default (use_reach) path equals the plain BFS: never wrong.
        let via_table =
            crate::blast::analyze_blast(&conn, "world", &crate::blast::BlastOptions::default())
                .unwrap();
        let via_bfs = crate::blast::analyze_blast(
            &conn,
            "world",
            &crate::blast::BlastOptions {
                use_reach: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(via_table, via_bfs);
        let names: Vec<&str> = via_table
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert!(names.contains(&"g0"), "BFS still finds the callers");
    }

    /// The other side of the work-budget boundary: an edit whose rebuild
    /// set is exactly `MAX_INCREMENTAL_REPAIR_SOURCES` names repairs
    /// incrementally — no stale marker, no degrade, table still answers.
    #[test]
    fn test_reindex_at_budget_repair_stays_incremental() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        // g-fns + world => a rebuild set of exactly g_fns + 1 = MAX names.
        fs::write(
            root.join("wide.rs"),
            wide_rust_source(crate::reach::MAX_INCREMENTAL_REPAIR_SOURCES - 1),
        )
        .unwrap();
        build_index(root, true).unwrap();
        let conn = db::open_existing(&db::local_index_path(root)).unwrap();

        let wide = root.join("wide.rs");
        let base = fs::read_to_string(&wide).unwrap();
        fs::write(&wide, format!("{base}// at-budget edit\n")).unwrap();
        let changed = reindex_file(
            &conn,
            &wide,
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();

        assert!(changed);
        assert!(
            reach_meta_value(&conn, "stale").is_none(),
            "an at-budget repair is incremental, not a degrade"
        );
        assert!(
            crate::reach::lookup_upstream(&conn, "world", 3)
                .unwrap()
                .is_some(),
            "the table still answers after an at-budget repair"
        );
        crate::reach::assert_table_equivalent_to_bfs(&conn);
    }

    /// TASK-081 acceptance: after every edit in a realistic sequence, the
    /// reach table stays equivalent to the live BFS at every depth — no
    /// staleness, no drift. Five steps through the daemon's real path
    /// (reindex_file / remove_file): add caller, remove caller, rename
    /// symbol with a dangling cross-file caller, delete a mid-chain file,
    /// introduce a mutual-recursion cycle.
    #[test]
    fn test_edit_sequence_reach_stays_equivalent_to_bfs() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        fs::write(root.join("src/lib.rs"), "fn world() { 1 }\n").unwrap();
        fs::write(root.join("src/mid.rs"), "fn mid() { world(); }\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() { mid(); }\n").unwrap();
        fs::write(root.join("src/extra.rs"), "fn extra() { mid(); }\n").unwrap();

        build_index(root, true).unwrap();
        let conn = db::open_existing(&db::local_index_path(root)).unwrap();
        crate::reach::assert_table_equivalent_to_bfs(&conn);

        // Step 1: add a direct caller of world.
        fs::write(
            root.join("src/lib.rs"),
            "fn world() { 1 }\nfn direct() { world(); }\n",
        )
        .unwrap();
        reindex_file(
            &conn,
            &root.join("src/lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        crate::reach::assert_table_equivalent_to_bfs(&conn);

        // Step 2: remove the caller again.
        fs::write(root.join("src/lib.rs"), "fn world() { 1 }\n").unwrap();
        reindex_file(
            &conn,
            &root.join("src/lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        crate::reach::assert_table_equivalent_to_bfs(&conn);

        // Step 3: rename world -> planet. mid.rs keeps calling the old
        // name: a dangling-name reference whose caller edges must keep
        // answering via BFS (the renamed symbol has no table rows).
        fs::write(root.join("src/lib.rs"), "fn planet() { 1 }\n").unwrap();
        reindex_file(
            &conn,
            &root.join("src/lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        crate::reach::assert_table_equivalent_to_bfs(&conn);

        // Step 4: delete the mid-chain file.
        fs::remove_file(root.join("src/mid.rs")).unwrap();
        remove_file(&conn, &root.join("src/mid.rs"), root).unwrap();
        crate::reach::assert_table_equivalent_to_bfs(&conn);

        // Step 5: introduce a mutual-recursion cycle main <-> extra.
        fs::write(root.join("src/main.rs"), "fn main() { extra(); }\n").unwrap();
        fs::write(root.join("src/extra.rs"), "fn extra() { main(); }\n").unwrap();
        reindex_file(
            &conn,
            &root.join("src/main.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        reindex_file(
            &conn,
            &root.join("src/extra.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
        crate::reach::assert_table_equivalent_to_bfs(&conn);
    }

    #[test]
    fn test_drop_all_data_clears_reach() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(
            root.join("lib.rs"),
            "fn hello() { world(); }\nfn world() { 2 }",
        )
        .unwrap();
        build_index(root, true).unwrap();
        let conn = db::open_existing(&db::local_index_path(root)).unwrap();

        let before = reach_row_count(&conn);
        assert!(before > 0, "index should carry reach rows before the drop");

        drop_all_data(&conn).unwrap();

        assert_eq!(reach_row_count(&conn), 0);
        assert!(reach_meta_value(&conn, "built_depth").is_none());
        assert!(reach_meta_value(&conn, "stale").is_none());
    }

    #[test]
    fn test_rebuild_clears_term_stats() {
        let dir = make_test_repo();
        build_index(dir.path(), true).unwrap();

        let index_path = db::local_index_path(dir.path());
        let conn = db::open_existing(&index_path).unwrap();
        let count1: i64 = conn
            .query_row("SELECT COUNT(*) FROM term_stats", [], |row| row.get(0))
            .unwrap();
        drop(conn);

        // Rebuild over unchanged content: stats are regenerated, not doubled.
        rebuild_index(dir.path(), true).unwrap();
        let conn = db::open_existing(&index_path).unwrap();
        let count2: i64 = conn
            .query_row("SELECT COUNT(*) FROM term_stats", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count1, count2, "rebuild must replace, not duplicate, stats");

        assert_stats_match_disk(&conn, dir.path());
    }

    #[test]
    fn test_term_stats_and_files_row_same_transaction() {
        let (dir, conn) = setup_indexed_repo();
        let root = dir.path();

        let original = fs::read_to_string(root.join("lib.rs")).unwrap();

        // Corrupt lib.rs into invalid UTF-8.  reindex_file must fail while
        // reading — before any write — leaving the previous file row and
        // stats exactly as they were.
        fs::write(root.join("lib.rs"), [0xffu8, 0xfe, 0x00, 0x01]).unwrap();
        let result = reindex_file(
            &conn,
            &root.join("lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        );
        assert!(
            result.is_err(),
            "invalid UTF-8 content must fail the re-index"
        );

        let hash: String = conn
            .query_row("SELECT hash FROM files WHERE path = 'lib.rs'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let expected_hash = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(original.as_bytes()));
        assert_eq!(hash, expected_hash, "files row must be untouched");

        let expected = crate::tokenizer::term_frequencies(&original);
        let actual: HashMap<String, i64> = conn
            .prepare("SELECT term, tf FROM term_stats WHERE file = 'lib.rs'")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(
            actual.len(),
            expected.len(),
            "stats must keep the pre-failure term set"
        );
        for (term, tf) in &expected {
            assert_eq!(
                actual.get(term).copied(),
                Some(*tf as i64),
                "tf for '{term}'"
            );
        }
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
        reindex_file(
            &conn,
            &root.join("lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();

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
        let changed = reindex_file(
            &conn,
            &dir.path().join("lib.rs"),
            dir.path(),
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
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

        let changed = reindex_file(
            &conn,
            &root.join("lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
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
        let changed = reindex_file(
            &conn,
            &root.join("lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
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
        reindex_file(
            &conn,
            &root.join("lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();

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
        reindex_file(
            &conn,
            &root.join("lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();

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

        index_new_file(
            &conn,
            &root.join("new_file.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();

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
        index_new_file(
            &conn,
            &root.join("readme.txt"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();

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

        let result = process_events(
            &conn,
            &events,
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();

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

        let result = process_events(
            &conn,
            &events,
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
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
        let result = process_events(
            &conn,
            &events,
            dir.path(),
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
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
        let result = process_events(
            &conn,
            &events,
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
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

        let result = process_events(
            &conn,
            &events,
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
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
        reindex_file(
            &conn,
            &root.join("lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();

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
        let client = crate::embedding::OllamaProvider::with_base_url("http://127.0.0.1:19999");
        let progress_mode = crate::progress::ProgressMode::Silent;

        let emb_stats = build_embeddings(&conn, root, &client, progress_mode).unwrap();
        assert!(emb_stats.skipped, "should skip when Ollama is unreachable");
        assert_eq!(emb_stats.embedded_count, 0);
    }

    #[test]
    fn test_first_batch_failure_preserves_existing_embeddings() {
        let dir = make_test_repo();
        let root = dir.path();
        build_index(root, true).unwrap();
        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        let symbol_id: i64 = conn
            .query_row("SELECT id FROM symbols LIMIT 1", [], |row| row.get(0))
            .unwrap();
        embedding::store_embedding(
            &conn,
            &TwoDimProvider,
            symbol_id,
            "existing.rs",
            "old chunk",
            &[1.0, 0.0],
        )
        .unwrap();

        let stats = build_embeddings(
            &conn,
            root,
            &FailingProvider,
            crate::progress::ProgressMode::Silent,
        )
        .unwrap();

        assert!(stats.skipped);
        let stored: (String, String) = conn
            .query_row(
                "SELECT provider, chunk_text FROM embeddings WHERE symbol_id = ?1",
                [symbol_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, ("test".to_string(), "old chunk".to_string()));
    }

    #[test]
    fn test_provider_switch_reembeds_and_propagates_metadata() {
        let dir = make_test_repo();
        let root = dir.path();
        build_index(root, true).unwrap();
        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        let symbol_id: i64 = conn
            .query_row("SELECT id FROM symbols LIMIT 1", [], |row| row.get(0))
            .unwrap();
        embedding::store_embedding(
            &conn,
            &OldTwoDimProvider,
            symbol_id,
            "existing.rs",
            "old chunk",
            &[0.0, 1.0],
        )
        .unwrap();

        let stats = build_missing_embeddings(
            &conn,
            root,
            &TwoDimProvider,
            crate::progress::ProgressMode::Silent,
        )
        .unwrap();
        assert!(stats.embedded_count > 0);

        let incompatible: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE provider != 'test' OR dim != 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(incompatible, 0);
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
        crate::embedding::store_embedding(
            &conn,
            &TwoDimProvider,
            sym_id,
            "test.rs",
            "chunk",
            &[1.0, 0.0],
        )
        .unwrap();

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
        let client = crate::embedding::OllamaProvider::with_base_url("http://127.0.0.1:19999");
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

        let client = crate::embedding::OllamaProvider::with_base_url("http://127.0.0.1:19999");
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
        embedding::store_embedding(
            &conn,
            &TwoDimProvider,
            sym_id,
            "src/main.rs",
            "chunk",
            &[1.0, 0.0],
        )
        .unwrap();

        // Verify embedding is fresh (not stale).
        let (_, stale_before) = embedding::embedding_stats(&conn).unwrap();
        assert_eq!(stale_before, 0, "embedding should be fresh initially");

        // Use dead port to simulate Ollama unreachable.
        let client = embedding::OllamaProvider::with_base_url("http://127.0.0.1:19999");
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

        let client = embedding::OllamaProvider::with_base_url("http://127.0.0.1:19999");
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
        let client = embedding::OllamaProvider::with_base_url("http://127.0.0.1:19999");
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
        reindex_file(
            &conn,
            &root.join("src/lib.rs"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();

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

        let changed = reindex_file(
            &conn,
            &root.join("app.ts"),
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
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
        let result = process_events(
            &conn,
            &events,
            root,
            &crate::contracts::ContractOptions::default(),
        )
        .unwrap();
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

    // -----------------------------------------------------------------------
    // Benchmark: term_stats build-time overhead (TASK-078)
    // -----------------------------------------------------------------------

    /// Pick a vocabulary index with a Zipf-like skew toward small indices
    /// (frequent head words, long tail of rare ones).
    fn zipf_pick(rng: &mut rand::rngs::StdRng, vocab_len: usize) -> usize {
        use rand::Rng;
        let u: f64 = rng.r#gen();
        ((vocab_len as f64) * u * u).floor() as usize % vocab_len
    }

    /// Build the fixed synthetic corpus used by the benchmark: 300 `.rs`
    /// files of ~150 lines each, tokens drawn from a 500-word Zipf-ish
    /// vocabulary, seeded so every run measures the identical corpus.
    fn write_bench_corpus(root: &Path) {
        use rand::SeedableRng;

        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("src")).unwrap();

        let mut rng = rand::rngs::StdRng::seed_from_u64(78);
        let vocab: Vec<String> = (0..500).map(|i| format!("w{i}")).collect();
        for file_idx in 0..300 {
            let mut lines = vec![format!("fn w{file_idx}_entry() {{")];
            while lines.len() < 150 {
                let picks: Vec<&str> = (0..6)
                    .map(|_| vocab[zipf_pick(&mut rng, vocab.len())].as_str())
                    .collect();
                lines.push(format!("    let value = {} + {};", picks[0], picks[1]));
                lines.push(format!(
                    "    call_{}({}, {});",
                    picks[2], picks[3], picks[4]
                ));
                if lines.len() >= 150 {
                    break;
                }
                lines.push(format!("    // {} {} {}", picks[5], picks[0], picks[2]));
            }
            lines.push("}".to_string());
            fs::write(
                root.join("src").join(format!("mod_{file_idx:03}.rs")),
                lines.join("\n"),
            )
            .unwrap();
        }
    }

    /// Run `build_index` three times on a fresh database, printing per-run
    /// elapsed and the median.  Returns the sorted durations.
    fn bench_three_fresh_builds(root: &Path) -> Vec<std::time::Duration> {
        let mut durations = Vec::new();
        for run in 0..3 {
            let index_dir = root.join(".wonk");
            if index_dir.exists() {
                fs::remove_dir_all(&index_dir).unwrap();
            }
            let start = std::time::Instant::now();
            let stats = build_index(root, true).unwrap();
            let elapsed = start.elapsed();
            durations.push(elapsed);
            println!(
                "bench run {}: {:?} ({} files, {} symbols)",
                run + 1,
                elapsed,
                stats.file_count,
                stats.symbol_count
            );
        }
        durations.sort();
        println!("bench median: {:?}", durations[1]);
        durations
    }

    /// Print the built index's DB size and `term_stats` row count.
    fn print_bench_db_stats(root: &Path) {
        let index_path = db::local_index_path(root);
        let db_size = fs::metadata(&index_path).map(|m| m.len()).unwrap_or(0);
        println!("bench index db size: {db_size} bytes");

        // term_stats may not exist yet (baseline run before TASK-078 writes).
        let conn = db::open_existing(&index_path).unwrap();
        let has_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='term_stats'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let term_stats_rows: i64 = if has_table > 0 {
            conn.query_row("SELECT COUNT(*) FROM term_stats", [], |row| row.get(0))
                .unwrap()
        } else {
            0
        };
        println!("bench term_stats rows: {term_stats_rows}");
    }

    /// Measure `build_index` on the synthetic corpus.  No timing assertion —
    /// this is a measurement harness, run manually via
    /// `cargo test --release bench_build_index_term_stats_overhead -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_build_index_term_stats_overhead() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write_bench_corpus(root);

        bench_three_fresh_builds(root);
        print_bench_db_stats(root);
    }

    /// Real-repo cross-check: build the index over this checkout itself —
    /// the index-only counterpart to `wonk init --local` (which also runs
    /// the embedding pass).  Removes the generated `.wonk` directory so
    /// the tree stays clean.
    #[test]
    #[ignore]
    fn bench_real_repo_build_index() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        bench_three_fresh_builds(&root);
        print_bench_db_stats(&root);

        fs::remove_dir_all(root.join(".wonk")).unwrap();
    }
}
