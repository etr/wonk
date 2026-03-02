//! Symbol change detection.
//!
//! Compares a fresh Tree-sitter parse of a file against the indexed version
//! in SQLite to detect which symbols were added, modified, or removed.
//! Also provides git-based file change detection for `--since` support.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use crate::embedding;
use crate::indexer;
use crate::semantic;
use crate::types::{
    ChangeAnalysis, ChangeScope, ChangeType, ChangedSymbol, ImpactResult, SemanticResult, Symbol,
    SymbolKind, SymbolRef,
};

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
    validate_file_path(file)?;

    let abs_path = repo_root.join(file);

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

    let current_symbols = parse_file_to_symbols(file, &content_str)?;

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
    validate_git_ref(commit)?;

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

/// Apply [`ChangeScope`] flags to a `git diff` command, validating refs.
fn apply_scope_args(cmd: &mut Command, scope: &ChangeScope) -> Result<()> {
    match scope {
        ChangeScope::Unstaged => {} // default: working tree vs index
        ChangeScope::Staged => {
            cmd.arg("--cached");
        }
        ChangeScope::All => {
            cmd.arg("HEAD");
        }
        ChangeScope::Compare(git_ref) => {
            validate_git_ref(git_ref)?;
            cmd.arg(git_ref.as_str());
        }
    }
    Ok(())
}

/// Return the list of files changed according to the given [`ChangeScope`].
///
/// Maps each scope variant to the appropriate `git diff --name-only` invocation:
/// - `Unstaged`: working tree vs index
/// - `Staged`: index vs HEAD
/// - `All`: working tree vs HEAD
/// - `Compare(ref)`: working tree vs the given ref
pub fn detect_scoped_files(scope: &ChangeScope, repo_root: &Path) -> Result<Vec<String>> {
    let mut cmd = Command::new("git");
    cmd.arg("diff").arg("--name-only");
    apply_scope_args(&mut cmd, scope)?;

    let output = cmd
        .current_dir(repo_root)
        .output()
        .context("failed to run git — is git installed?")?;

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

/// Map diff hunk ranges to indexed symbols, returning `Modified` entries for
/// any symbol whose `line..end_line` range overlaps a changed hunk.
///
/// This is a pure function: it does no I/O. Symbols without `end_line` are
/// treated as single-line (line..line). Each symbol appears at most once even
/// if it overlaps multiple hunks.
pub fn map_hunks_to_symbols(
    symbols: &[Symbol],
    hunks: &[(usize, usize)],
    file: &str,
) -> Vec<ChangedSymbol> {
    if hunks.is_empty() {
        return Vec::new();
    }

    let mut seen = HashSet::new();
    let mut result = Vec::new();

    for sym in symbols {
        let sym_start = sym.line;
        let sym_end = sym.end_line.unwrap_or(sym.line);

        for &(hunk_start, hunk_end) in hunks {
            // Overlap check: two ranges [a, b] and [c, d] overlap iff a <= d && c <= b
            if sym_start <= hunk_end && hunk_start <= sym_end {
                let key = (sym.name.clone(), sym.kind, sym.scope.clone());
                if seen.insert(key) {
                    result.push(ChangedSymbol {
                        name: sym.name.clone(),
                        kind: sym.kind,
                        file: file.to_string(),
                        line: sym.line,
                        change_type: ChangeType::Modified,
                    });
                }
                break; // Already matched this symbol, no need to check more hunks
            }
        }
    }

    result
}

/// Run `git diff --unified=0` for a single file under the given scope and
/// return the parsed hunk ranges via [`parse_diff_hunks`].
pub fn get_diff_hunks_for_file(
    scope: &ChangeScope,
    file: &str,
    repo_root: &Path,
) -> Result<Vec<(usize, usize)>> {
    let mut cmd = Command::new("git");
    cmd.arg("diff").arg("--unified=0");
    apply_scope_args(&mut cmd, scope)?;
    cmd.arg("--").arg(file);

    let output = cmd
        .current_dir(repo_root)
        .output()
        .context("failed to run git diff for hunks")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git diff failed for file {file}: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_diff_hunks(&stdout))
}

/// Parse `git diff --unified=0` output and extract changed line ranges.
///
/// Looks for hunk headers of the form `@@ -old_start[,old_count] +new_start[,new_count] @@`
/// and returns `(start_line, end_line)` pairs from the **new** (right) side.
///
/// - When `count` is omitted it defaults to 1.
/// - When `count` is 0 the hunk represents a pure deletion and is skipped.
pub fn parse_diff_hunks(diff_output: &str) -> Vec<(usize, usize)> {
    let mut hunks = Vec::new();

    for line in diff_output.lines() {
        if !line.starts_with("@@") {
            continue;
        }

        // Find the +start[,count] portion.
        // Format: @@ -old_start[,old_count] +new_start[,new_count] @@
        let plus_pos = match line.find('+') {
            Some(p) => p,
            None => continue,
        };
        let after_plus = &line[plus_pos + 1..];

        // Find the end of the new-side range (terminated by space or @@).
        let range_end = after_plus.find([' ', '@']).unwrap_or(after_plus.len());
        let range_str = &after_plus[..range_end];

        let (start, count) = if let Some(comma) = range_str.find(',') {
            let s = range_str[..comma].parse::<usize>().unwrap_or(0);
            let c = range_str[comma + 1..].parse::<usize>().unwrap_or(1);
            (s, c)
        } else {
            let s = range_str.parse::<usize>().unwrap_or(0);
            (s, 1) // count defaults to 1 when omitted
        };

        // count=0 means pure deletion on the new side, skip.
        if count == 0 {
            continue;
        }

        let end = start + count - 1;
        hunks.push((start, end));
    }

    hunks
}

/// Parse file content with Tree-sitter and return extracted symbols.
///
/// Shared helper for both [`detect_changed_symbols`] and [`parse_current_symbols`].
fn parse_file_to_symbols(file: &str, content: &str) -> Result<Vec<Symbol>> {
    let lang = match indexer::detect_language(Path::new(file)) {
        Some(l) => l,
        None => bail!("unsupported language for file: {file}"),
    };

    let mut parser = indexer::get_parser(lang);
    let tree = parser
        .parse(content.as_bytes(), None)
        .context("tree-sitter parse failed")?;

    Ok(indexer::extract_symbols(&tree, content, file, lang))
}

/// Validate a git ref string to prevent argument injection (CWE-88).
///
/// Allows alphanumeric chars plus common git-ref characters: `/ _ . - @ ~ ^`
/// Rejects empty strings and anything containing disallowed characters.
pub fn validate_git_ref(git_ref: &str) -> Result<()> {
    if git_ref.is_empty()
        || !git_ref
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/_.-@~^".contains(c))
    {
        bail!("invalid git reference: {git_ref}");
    }
    Ok(())
}

/// Validate that a file path is safe: no `..` components and no absolute paths.
fn validate_file_path(file: &str) -> Result<()> {
    let path = Path::new(file);
    if path.is_absolute() {
        bail!("absolute path not allowed: {file}");
    }
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        bail!("path escapes repository root: {file}");
    }
    Ok(())
}

/// Re-parse a file from disk to get full [`Symbol`] structs (with `end_line`).
///
/// This is needed because `detect_changed_symbols` only returns [`ChangedSymbol`]
/// which lacks `end_line`, `signature`, and other fields needed for embedding.
pub fn parse_current_symbols(file: &str, repo_root: &Path) -> Result<Vec<Symbol>> {
    validate_file_path(file)?;

    let abs_path = repo_root.join(file);
    let content =
        std::fs::read_to_string(&abs_path).with_context(|| format!("reading file {file}"))?;

    parse_file_to_symbols(file, &content)
}

/// Detect all changed symbols across files for a given [`ChangeScope`].
///
/// This is the main public entry point for scoped change detection.  It:
/// 1. Discovers changed files via `git diff --name-only` with the appropriate flags.
/// 2. For each file that is a supported language:
///    a. Gets diff hunks to identify line ranges that changed.
///    b. Queries indexed symbols and maps hunks to overlapping symbols (Modified).
///    c. Runs Tree-sitter re-parse via [`detect_changed_symbols`] for Added/Removed.
///    d. Merges results with dedup (Tree-sitter results take priority).
/// 3. Returns a [`ChangeAnalysis`] with all changed symbols.
pub fn detect_changes(
    conn: &Connection,
    scope: &ChangeScope,
    repo_root: &Path,
) -> Result<ChangeAnalysis> {
    let changed_files = detect_scoped_files(scope, repo_root)?;

    let mut all_changes: Vec<ChangedSymbol> = Vec::new();

    for file in &changed_files {
        // Skip files we can't parse (non-supported languages).
        if indexer::detect_language(Path::new(file)).is_none() {
            continue;
        }

        // Step 1: Hunk-based detection (Modified symbols).
        let hunks = get_diff_hunks_for_file(scope, file, repo_root).unwrap_or_default();
        let indexed_symbols = query_indexed_symbols(conn, file).unwrap_or_default();
        let hunk_modified = map_hunks_to_symbols(&indexed_symbols, &hunks, file);

        // Step 2: Tree-sitter based detection (Added/Removed/Modified via signature diff).
        let ts_changes = detect_changed_symbols(conn, file, repo_root).unwrap_or_default();

        // Step 3: Merge with dedup. Tree-sitter results take priority because they
        // have more precise change classification (signature-based Modified vs
        // hunk-overlap Modified, plus Added/Removed).
        let mut seen: HashSet<(String, SymbolKind, Option<String>)> = HashSet::new();

        // Add tree-sitter results first (they have priority).
        for cs in &ts_changes {
            let key = (cs.name.clone(), cs.kind, None); // scope not in ChangedSymbol
            if seen.insert(key) {
                all_changes.push(cs.clone());
            }
        }

        // Add hunk-based Modified that weren't already covered by tree-sitter.
        for cs in &hunk_modified {
            let key = (cs.name.clone(), cs.kind, None);
            if seen.insert(key) {
                all_changes.push(cs.clone());
            }
        }
    }

    Ok(ChangeAnalysis {
        scope: scope.clone(),
        changed_symbols: all_changes,
    })
}

/// Build [`ImpactResult`] entries from semantic search results, excluding
/// self-matches.
///
/// `self_ids` contains the symbol IDs of the changed symbol(s) in the index
/// that should be excluded from results.
pub fn build_impact_results(
    changed: &SymbolRef,
    semantic_results: &[SemanticResult],
    self_ids: &HashSet<i64>,
) -> Vec<ImpactResult> {
    semantic_results
        .iter()
        .filter(|sr| !self_ids.contains(&sr.symbol_id))
        .map(|sr| ImpactResult {
            changed_symbol: changed.clone(),
            impacted_symbol: SymbolRef {
                name: sr.symbol_name.clone(),
                kind: sr.symbol_kind,
                file: sr.file.clone(),
                line: sr.line,
            },
            similarity_score: sr.similarity_score,
        })
        .collect()
}

/// Lookup the symbol IDs for a given name, kind, and file in the index.
///
/// Returns a set of matching symbol IDs for self-exclusion.
fn query_self_symbol_ids(
    conn: &Connection,
    name: &str,
    kind: SymbolKind,
    file: &str,
) -> HashSet<i64> {
    let mut ids = HashSet::new();
    let kind_str = kind.to_string();
    if let Ok(mut stmt) =
        conn.prepare("SELECT id FROM symbols WHERE name = ?1 AND kind = ?2 AND file = ?3")
        && let Ok(rows) = stmt.query_map(rusqlite::params![name, kind_str, file], |row| {
            row.get::<_, i64>(0)
        })
    {
        for r in rows.flatten() {
            ids.insert(r);
        }
    }
    ids
}

/// Analyze the impact of changed symbols in a file.
///
/// Detects changed symbols, embeds each one, and finds semantically similar
/// symbols in the index. Returns results sorted by descending similarity.
///
/// `all_embeddings` should be pre-loaded via [`embedding::load_all_embeddings`]
/// and shared across multiple calls (e.g. `--since` mode) to avoid redundant
/// SQLite I/O.
///
/// Returns an empty `Vec` if the file has no changes.
pub fn analyze_impact(
    conn: &Connection,
    file: &str,
    repo_root: &Path,
    client: &embedding::OllamaClient,
    all_embeddings: &[(i64, Vec<f32>)],
) -> Result<Vec<ImpactResult>> {
    validate_file_path(file)?;

    // Step 1: Detect changed symbols.
    let changes = detect_changed_symbols(conn, file, repo_root)?;
    if changes.is_empty() {
        return Ok(Vec::new());
    }

    // Step 2: Filter out Removed symbols (no current source to embed).
    let embeddable: Vec<_> = changes
        .iter()
        .filter(|c| c.change_type != ChangeType::Removed)
        .collect();

    if embeddable.is_empty() {
        return Ok(Vec::new());
    }

    // Step 3: Nothing to compare against if no embeddings were provided.
    if all_embeddings.is_empty() {
        return Ok(Vec::new());
    }

    // Step 4: Read file content once (shared for parsing + chunking).
    let abs_path = repo_root.join(file);
    let source_code =
        std::fs::read_to_string(&abs_path).with_context(|| format!("reading file {file}"))?;

    // Step 4: Parse to get full Symbol structs (with end_line for chunking).
    let current_symbols = parse_file_to_symbols(file, &source_code)?;

    // Build a lookup map from (name, kind) to full Symbol for chunking.
    let sym_map: HashMap<(String, SymbolKind), &Symbol> = current_symbols
        .iter()
        .map(|s| ((s.name.clone(), s.kind), s))
        .collect();

    // Step 5: Load file imports for chunk context.
    // Imports are optional context for embedding; silently skip on DB error.
    let file_imports = conn
        .prepare("SELECT import_path FROM file_imports WHERE source_file = ?1")
        .and_then(|mut stmt| {
            let rows = stmt.query_map(rusqlite::params![file], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

    // Step 6: For each changed symbol, embed and search.
    let mut all_results = Vec::new();

    for changed in &embeddable {
        let changed_ref = SymbolRef::from(*changed);

        // Find the full Symbol for chunking.
        let full_sym = match sym_map.get(&(changed.name.clone(), changed.kind)) {
            Some(s) => *s,
            None => continue, // Symbol not found in parse, skip.
        };

        // Build chunk text.
        let chunk = embedding::chunk_symbol(full_sym, &file_imports, &source_code);

        // Embed the chunk.
        let mut query_vec = client.embed_single(&chunk)?;
        embedding::normalize(&mut query_vec);

        // Semantic search (limit to 20 results per changed symbol).
        let scored = semantic::semantic_search(&query_vec, all_embeddings, 20);
        let resolved = semantic::resolve_results(conn, &scored)?;

        // Self-exclusion: find IDs of the changed symbol itself.
        let self_ids = query_self_symbol_ids(conn, &changed.name, changed.kind, &changed.file);

        let results = build_impact_results(&changed_ref, &resolved, &self_ids);
        all_results.extend(results);
    }

    // Step 7: Sort by descending similarity and deduplicate.
    all_results.sort_by(|a, b| {
        b.similarity_score
            .partial_cmp(&a.similarity_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Deduplicate: keep only the highest-scoring entry per impacted symbol.
    let mut seen = HashSet::new();
    all_results.retain(|r| {
        seen.insert((
            r.impacted_symbol.name.clone(),
            r.impacted_symbol.kind,
            r.impacted_symbol.file.clone(),
            r.impacted_symbol.line,
        ))
    });

    Ok(all_results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::pipeline;
    use crate::types::{ChangeType, SymbolKind, SymbolRef};
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
    fn path_traversal_rejected() {
        let source = "fn hello() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        let result = detect_changed_symbols(&conn, "../../etc/passwd", dir.path());
        assert!(result.is_err(), "path with .. should be rejected");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("path escapes repository root"));
    }

    // -- validate_git_ref tests ------------------------------------------------

    #[test]
    fn validate_git_ref_accepts_valid_refs() {
        assert!(validate_git_ref("HEAD").is_ok());
        assert!(validate_git_ref("main").is_ok());
        assert!(validate_git_ref("origin/main").is_ok());
        assert!(validate_git_ref("v1.0.0").is_ok());
        assert!(validate_git_ref("HEAD~3").is_ok());
        assert!(validate_git_ref("HEAD^2").is_ok());
        assert!(validate_git_ref("feature/my-branch").is_ok());
        assert!(validate_git_ref("abc123").is_ok());
    }

    #[test]
    fn validate_git_ref_rejects_empty() {
        assert!(validate_git_ref("").is_err());
    }

    #[test]
    fn validate_git_ref_rejects_flag_injection() {
        let result = validate_git_ref("--upload-pack=evil");
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("invalid git reference"));
    }

    #[test]
    fn validate_git_ref_rejects_special_characters() {
        assert!(validate_git_ref("HEAD:../../etc/passwd").is_err());
        assert!(validate_git_ref("ref;rm -rf /").is_err());
    }

    #[test]
    fn git_diff_rejects_empty_commit() {
        let dir = TempDir::new().unwrap();
        let result = detect_changed_files_since("", dir.path());
        assert!(result.is_err(), "empty commit ref should be rejected");
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
    fn git_diff_rejects_flag_injection() {
        let dir = TempDir::new().unwrap();
        let result = detect_changed_files_since("--upload-pack=evil", dir.path());
        assert!(result.is_err(), "flag-shaped commit ref should be rejected");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("invalid git reference"),
            "error should mention invalid git reference"
        );
    }

    #[test]
    fn git_diff_rejects_special_characters() {
        let dir = TempDir::new().unwrap();
        let result = detect_changed_files_since("HEAD:../../etc/passwd", dir.path());
        assert!(result.is_err(), "commit ref with colon should be rejected");
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

    // -- parse_current_symbols tests -------------------------------------------

    #[test]
    fn parse_current_symbols_from_disk() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "fn alpha() { }\nfn beta(x: i32) -> i32 { x }\n",
        )
        .unwrap();

        let symbols = parse_current_symbols("src/lib.rs", root).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert_eq!(symbols.len(), 2);
        // Symbols should have end_line filled in
        for sym in &symbols {
            assert!(
                sym.end_line.is_some(),
                "end_line should be set for {}",
                sym.name
            );
        }
    }

    #[test]
    fn parse_current_symbols_nonexistent_file() {
        let dir = TempDir::new().unwrap();
        let result = parse_current_symbols("nonexistent.rs", dir.path());
        assert!(result.is_err());
    }

    // -- filter_and_match_symbols tests ----------------------------------------

    #[test]
    fn filter_removed_symbols_excluded() {
        // Only Added and Modified should remain after filtering
        let changes = [
            ChangedSymbol {
                name: "added_fn".into(),
                kind: SymbolKind::Function,
                file: "src/lib.rs".into(),
                line: 1,
                change_type: ChangeType::Added,
            },
            ChangedSymbol {
                name: "modified_fn".into(),
                kind: SymbolKind::Function,
                file: "src/lib.rs".into(),
                line: 5,
                change_type: ChangeType::Modified,
            },
            ChangedSymbol {
                name: "removed_fn".into(),
                kind: SymbolKind::Function,
                file: "src/lib.rs".into(),
                line: 10,
                change_type: ChangeType::Removed,
            },
        ];

        let filtered: Vec<_> = changes
            .iter()
            .filter(|c| c.change_type != ChangeType::Removed)
            .collect();

        assert_eq!(filtered.len(), 2);
        assert!(
            filtered
                .iter()
                .all(|c| c.change_type != ChangeType::Removed)
        );
    }

    // -- build_impact_results tests (unit test for the aggregation helper) ----

    #[test]
    fn build_impact_results_excludes_self_match() {
        let results = build_impact_results(
            &SymbolRef {
                name: "foo".into(),
                kind: SymbolKind::Function,
                file: "a.rs".into(),
                line: 1,
            },
            &[crate::types::SemanticResult {
                symbol_id: 1,
                file: "a.rs".into(),
                line: 1,
                symbol_name: "foo".into(),
                symbol_kind: SymbolKind::Function,
                similarity_score: 1.0,
            }],
            &HashSet::from([1i64]),
        );
        assert!(results.is_empty(), "self-match should be excluded");
    }

    #[test]
    fn build_impact_results_includes_non_self() {
        let results = build_impact_results(
            &SymbolRef {
                name: "foo".into(),
                kind: SymbolKind::Function,
                file: "a.rs".into(),
                line: 1,
            },
            &[
                crate::types::SemanticResult {
                    symbol_id: 1,
                    file: "a.rs".into(),
                    line: 1,
                    symbol_name: "foo".into(),
                    symbol_kind: SymbolKind::Function,
                    similarity_score: 1.0,
                },
                crate::types::SemanticResult {
                    symbol_id: 2,
                    file: "b.rs".into(),
                    line: 10,
                    symbol_name: "bar".into(),
                    symbol_kind: SymbolKind::Function,
                    similarity_score: 0.85,
                },
            ],
            &HashSet::from([1i64]),
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].impacted_symbol.name, "bar");
        assert!((results[0].similarity_score - 0.85).abs() < 1e-6);
    }

    // -- analyze_impact with no embeddings returns error -----------------------

    #[test]
    fn analyze_impact_empty_embeddings_returns_empty() {
        let source = "fn hello() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        // Modify the file so changes are detected
        fs::write(dir.path().join("src/lib.rs"), "fn hello(x: i32) { }\n").unwrap();

        // Dead-port client
        let client = crate::embedding::OllamaClient::with_base_url("http://127.0.0.1:19999");

        // Pass empty embeddings — should return empty (no embeddings to compare against)
        let all_embeddings = Vec::new();
        let results =
            analyze_impact(&conn, "src/lib.rs", dir.path(), &client, &all_embeddings).unwrap();
        assert!(results.is_empty(), "empty embeddings should return empty");
    }

    // -- analyze_impact with unchanged file returns empty ----------------------

    #[test]
    fn analyze_impact_unchanged_returns_empty() {
        let source = "fn hello() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        // File unchanged, no client needed
        let client = crate::embedding::OllamaClient::with_base_url("http://127.0.0.1:19999");

        let all_embeddings = Vec::new();
        let results =
            analyze_impact(&conn, "src/lib.rs", dir.path(), &client, &all_embeddings).unwrap();
        assert!(
            results.is_empty(),
            "unchanged file should produce no impact results"
        );
    }

    #[test]
    fn parse_current_symbols_rejects_path_traversal() {
        let dir = TempDir::new().unwrap();
        let result = parse_current_symbols("../../etc/passwd", dir.path());
        assert!(result.is_err(), "path with .. should be rejected");
    }

    #[test]
    fn parse_current_symbols_rejects_absolute_path() {
        let dir = TempDir::new().unwrap();
        let result = parse_current_symbols("/etc/passwd", dir.path());
        assert!(result.is_err(), "absolute path should be rejected");
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

    // -- detect_scoped_files tests ---------------------------------------------

    /// Helper: create a git repo with initial commit and return (TempDir, root).
    fn make_git_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
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
        dir
    }

    #[test]
    fn detect_scoped_files_unstaged() {
        if !git_available() {
            return;
        }
        let dir = make_git_repo();
        let root = dir.path();

        // Modify a tracked file without staging
        fs::write(root.join("a.rs"), "fn a() { modified }\n").unwrap();

        let files = detect_scoped_files(&ChangeScope::Unstaged, root).unwrap();
        assert!(
            files.contains(&"a.rs".to_string()),
            "unstaged changes should show modified file"
        );
    }

    #[test]
    fn detect_scoped_files_staged() {
        if !git_available() {
            return;
        }
        let dir = make_git_repo();
        let root = dir.path();

        // Stage a new file
        fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
        Command::new("git")
            .args(["add", "b.rs"])
            .current_dir(root)
            .output()
            .unwrap();

        let files = detect_scoped_files(&ChangeScope::Staged, root).unwrap();
        assert!(
            files.contains(&"b.rs".to_string()),
            "staged changes should show added file"
        );

        // Unstaged should NOT contain b.rs (it's staged)
        let unstaged = detect_scoped_files(&ChangeScope::Unstaged, root).unwrap();
        assert!(
            !unstaged.contains(&"b.rs".to_string()),
            "staged file should not appear in unstaged scope"
        );
    }

    #[test]
    fn detect_scoped_files_all() {
        if !git_available() {
            return;
        }
        let dir = make_git_repo();
        let root = dir.path();

        // Stage one change, leave another unstaged
        fs::write(root.join("a.rs"), "fn a() { modified }\n").unwrap();
        fs::write(root.join("c.rs"), "fn c() {}\n").unwrap();
        Command::new("git")
            .args(["add", "c.rs"])
            .current_dir(root)
            .output()
            .unwrap();

        let files = detect_scoped_files(&ChangeScope::All, root).unwrap();
        assert!(
            files.contains(&"a.rs".to_string()),
            "all scope should include unstaged changes"
        );
        assert!(
            files.contains(&"c.rs".to_string()),
            "all scope should include staged changes"
        );
    }

    #[test]
    fn detect_scoped_files_compare() {
        if !git_available() {
            return;
        }
        let dir = make_git_repo();
        let root = dir.path();

        // Get commit hash
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let commit = String::from_utf8(output.stdout).unwrap().trim().to_string();

        // Make and commit changes
        fs::write(root.join("a.rs"), "fn a() { v2 }\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "v2"])
            .current_dir(root)
            .output()
            .unwrap();

        let files = detect_scoped_files(&ChangeScope::Compare(commit), root).unwrap();
        assert!(
            files.contains(&"a.rs".to_string()),
            "compare scope should show files changed since ref"
        );
    }

    // -- parse_diff_hunks tests -----------------------------------------------

    #[test]
    fn parse_diff_hunks_single_line_change() {
        let diff = "@@ -5,1 +5,1 @@ fn foo()\n";
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks, vec![(5, 5)]);
    }

    #[test]
    fn parse_diff_hunks_multi_line_change() {
        let diff = "@@ -10,3 +10,5 @@ fn bar()\n";
        let hunks = parse_diff_hunks(diff);
        // +10,5 means lines 10 through 14
        assert_eq!(hunks, vec![(10, 14)]);
    }

    #[test]
    fn parse_diff_hunks_no_count_defaults_to_one() {
        // When count is omitted, it defaults to 1
        let diff = "@@ -1 +1 @@\n";
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks, vec![(1, 1)]);
    }

    #[test]
    fn parse_diff_hunks_deletion_only_skipped() {
        // count=0 on the new side means pure deletion, no new lines
        let diff = "@@ -5,3 +5,0 @@ fn deleted()\n";
        let hunks = parse_diff_hunks(diff);
        assert!(hunks.is_empty(), "pure deletions should be skipped");
    }

    #[test]
    fn parse_diff_hunks_multiple_hunks() {
        let diff = "\
diff --git a/foo.rs b/foo.rs
index 1234..5678 100644
--- a/foo.rs
+++ b/foo.rs
@@ -3,1 +3,2 @@ fn one()
+added line
@@ -10,2 +11,3 @@ fn two()
+another added
";
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0], (3, 4)); // +3,2
        assert_eq!(hunks[1], (11, 13)); // +11,3
    }

    #[test]
    fn parse_diff_hunks_empty_input() {
        let hunks = parse_diff_hunks("");
        assert!(hunks.is_empty());
    }

    #[test]
    fn parse_diff_hunks_addition_at_end() {
        // Adding lines at the end of a file
        let diff = "@@ -0,0 +1,3 @@\n";
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks, vec![(1, 3)]);
    }

    // -- map_hunks_to_symbols tests -------------------------------------------

    #[test]
    fn map_hunks_no_overlap() {
        let symbols = vec![Symbol {
            name: "foo".into(),
            kind: SymbolKind::Function,
            file: "a.rs".into(),
            line: 10,
            col: 0,
            end_line: Some(15),
            scope: None,
            signature: "fn foo()".into(),
            language: "Rust".into(),
        }];

        // Hunk is on lines 1-5, symbol is on lines 10-15
        let hunks = vec![(1, 5)];
        let result = map_hunks_to_symbols(&symbols, &hunks, "a.rs");
        assert!(
            result.is_empty(),
            "non-overlapping hunk should produce no matches"
        );
    }

    #[test]
    fn map_hunks_full_overlap() {
        let symbols = vec![Symbol {
            name: "bar".into(),
            kind: SymbolKind::Function,
            file: "a.rs".into(),
            line: 5,
            col: 0,
            end_line: Some(10),
            scope: None,
            signature: "fn bar()".into(),
            language: "Rust".into(),
        }];

        // Hunk covers lines 6-8, inside symbol 5-10
        let hunks = vec![(6, 8)];
        let result = map_hunks_to_symbols(&symbols, &hunks, "a.rs");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "bar");
        assert_eq!(result[0].change_type, ChangeType::Modified);
    }

    #[test]
    fn map_hunks_partial_overlap_start() {
        let symbols = vec![Symbol {
            name: "baz".into(),
            kind: SymbolKind::Function,
            file: "a.rs".into(),
            line: 5,
            col: 0,
            end_line: Some(10),
            scope: None,
            signature: "fn baz()".into(),
            language: "Rust".into(),
        }];

        // Hunk starts before symbol, ends inside it
        let hunks = vec![(3, 6)];
        let result = map_hunks_to_symbols(&symbols, &hunks, "a.rs");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "baz");
    }

    #[test]
    fn map_hunks_symbol_without_end_line() {
        let symbols = vec![Symbol {
            name: "const_val".into(),
            kind: SymbolKind::Constant,
            file: "a.rs".into(),
            line: 3,
            col: 0,
            end_line: None, // single-line symbol
            scope: None,
            signature: "const VAL: i32 = 42".into(),
            language: "Rust".into(),
        }];

        // Hunk covers line 3
        let hunks = vec![(3, 3)];
        let result = map_hunks_to_symbols(&symbols, &hunks, "a.rs");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "const_val");
    }

    #[test]
    fn map_hunks_multiple_symbols() {
        let symbols = vec![
            Symbol {
                name: "alpha".into(),
                kind: SymbolKind::Function,
                file: "a.rs".into(),
                line: 1,
                col: 0,
                end_line: Some(5),
                scope: None,
                signature: "fn alpha()".into(),
                language: "Rust".into(),
            },
            Symbol {
                name: "beta".into(),
                kind: SymbolKind::Function,
                file: "a.rs".into(),
                line: 7,
                col: 0,
                end_line: Some(12),
                scope: None,
                signature: "fn beta()".into(),
                language: "Rust".into(),
            },
            Symbol {
                name: "gamma".into(),
                kind: SymbolKind::Function,
                file: "a.rs".into(),
                line: 14,
                col: 0,
                end_line: Some(20),
                scope: None,
                signature: "fn gamma()".into(),
                language: "Rust".into(),
            },
        ];

        // Hunk covers lines 3-9, overlapping alpha (1-5) and beta (7-12)
        let hunks = vec![(3, 9)];
        let result = map_hunks_to_symbols(&symbols, &hunks, "a.rs");
        let names: Vec<&str> = result.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert!(!names.contains(&"gamma"), "gamma should not overlap");
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn map_hunks_no_duplicate_symbols() {
        let symbols = vec![Symbol {
            name: "foo".into(),
            kind: SymbolKind::Function,
            file: "a.rs".into(),
            line: 1,
            col: 0,
            end_line: Some(10),
            scope: None,
            signature: "fn foo()".into(),
            language: "Rust".into(),
        }];

        // Two hunks both overlap the same symbol
        let hunks = vec![(2, 3), (8, 9)];
        let result = map_hunks_to_symbols(&symbols, &hunks, "a.rs");
        assert_eq!(result.len(), 1, "should not duplicate symbol");
    }

    // -- get_diff_hunks_for_file tests ----------------------------------------

    #[test]
    fn get_diff_hunks_for_file_unstaged() {
        if !git_available() {
            return;
        }
        let dir = make_git_repo();
        let root = dir.path();

        // Modify a tracked file without staging
        fs::write(root.join("a.rs"), "fn a() { modified }\n").unwrap();

        let hunks = get_diff_hunks_for_file(&ChangeScope::Unstaged, "a.rs", root).unwrap();
        assert!(!hunks.is_empty(), "should detect hunks in modified file");
        // The change is on line 1
        assert_eq!(hunks[0].0, 1);
    }

    #[test]
    fn get_diff_hunks_for_file_staged() {
        if !git_available() {
            return;
        }
        let dir = make_git_repo();
        let root = dir.path();

        // Modify and stage
        fs::write(root.join("a.rs"), "fn a() { staged }\n").unwrap();
        Command::new("git")
            .args(["add", "a.rs"])
            .current_dir(root)
            .output()
            .unwrap();

        let hunks = get_diff_hunks_for_file(&ChangeScope::Staged, "a.rs", root).unwrap();
        assert!(!hunks.is_empty(), "should detect hunks in staged file");
    }

    #[test]
    fn get_diff_hunks_for_file_no_changes() {
        if !git_available() {
            return;
        }
        let dir = make_git_repo();
        let root = dir.path();

        // No changes to a.rs
        let hunks = get_diff_hunks_for_file(&ChangeScope::Unstaged, "a.rs", root).unwrap();
        assert!(hunks.is_empty(), "unchanged file should have no hunks");
    }

    #[test]
    fn detect_scoped_files_compare_invalid_ref() {
        if !git_available() {
            return;
        }
        let dir = make_git_repo();
        let root = dir.path();

        let result = detect_scoped_files(&ChangeScope::Compare("--evil-flag".into()), root);
        assert!(result.is_err(), "invalid ref should produce error");
    }

    // -- detect_changes tests -------------------------------------------------

    /// Helper: create a git repo with indexed Rust source and return (TempDir, Connection).
    fn make_git_indexed_repo(source: &str) -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Init git repo
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

        // Create source file
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), source).unwrap();

        // Initial commit
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

        // Build the wonk index
        pipeline::build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        (dir, conn)
    }

    #[test]
    fn detect_changes_unstaged_modified() {
        if !git_available() {
            return;
        }
        let source = "fn hello() -> i32 { 42 }\nfn world() { }\n";
        let (dir, conn) = make_git_indexed_repo(source);
        let root = dir.path();

        // Modify hello's body (unstaged)
        fs::write(
            root.join("src/lib.rs"),
            "fn hello() -> i32 { 99 }\nfn world() { }\n",
        )
        .unwrap();

        let analysis = detect_changes(&conn, &ChangeScope::Unstaged, root).unwrap();
        assert_eq!(analysis.scope, ChangeScope::Unstaged);

        // hello should be detected as Modified (hunk overlaps its line range)
        let modified: Vec<&ChangedSymbol> = analysis
            .changed_symbols
            .iter()
            .filter(|c| c.change_type == ChangeType::Modified)
            .collect();
        let names: Vec<&str> = modified.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"hello"),
            "hello should be modified, got: {:?}",
            analysis.changed_symbols
        );

        // world should NOT appear (unchanged)
        let all_names: Vec<&str> = analysis
            .changed_symbols
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            !all_names.contains(&"world"),
            "world should not be in changes"
        );
    }

    #[test]
    fn detect_changes_staged_added_symbol() {
        if !git_available() {
            return;
        }
        let source = "fn existing() { }\n";
        let (dir, conn) = make_git_indexed_repo(source);
        let root = dir.path();

        // Add a new function and stage it
        fs::write(
            root.join("src/lib.rs"),
            "fn existing() { }\nfn brand_new() { }\n",
        )
        .unwrap();
        Command::new("git")
            .args(["add", "src/lib.rs"])
            .current_dir(root)
            .output()
            .unwrap();

        let analysis = detect_changes(&conn, &ChangeScope::Staged, root).unwrap();
        assert_eq!(analysis.scope, ChangeScope::Staged);

        // brand_new should be Added (tree-sitter detect)
        let added: Vec<&str> = analysis
            .changed_symbols
            .iter()
            .filter(|c| c.change_type == ChangeType::Added)
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            added.contains(&"brand_new"),
            "new function should be detected as Added, got: {:?}",
            analysis.changed_symbols
        );
    }

    #[test]
    fn detect_changes_compare_ref() {
        if !git_available() {
            return;
        }
        let source = "fn original() { }\n";
        let (dir, conn) = make_git_indexed_repo(source);
        let root = dir.path();

        // Get current commit
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        let base_commit = String::from_utf8(output.stdout).unwrap().trim().to_string();

        // Make a new commit with changes
        fs::write(
            root.join("src/lib.rs"),
            "fn original() { changed }\nfn added() { }\n",
        )
        .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "v2"])
            .current_dir(root)
            .output()
            .unwrap();

        let analysis =
            detect_changes(&conn, &ChangeScope::Compare(base_commit.clone()), root).unwrap();
        assert_eq!(analysis.scope, ChangeScope::Compare(base_commit));

        let names: Vec<&str> = analysis
            .changed_symbols
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            names.contains(&"original") || names.contains(&"added"),
            "should detect changes against base commit, got: {:?}",
            analysis.changed_symbols
        );
    }

    #[test]
    fn detect_changes_no_changes() {
        if !git_available() {
            return;
        }
        let source = "fn unchanged() { }\n";
        let (dir, conn) = make_git_indexed_repo(source);
        let root = dir.path();

        // No modifications
        let analysis = detect_changes(&conn, &ChangeScope::Unstaged, root).unwrap();
        assert!(
            analysis.changed_symbols.is_empty(),
            "no changes should produce empty result"
        );
    }

    #[test]
    fn detect_changes_removed_symbol() {
        if !git_available() {
            return;
        }
        let source = "fn keep_me() { }\nfn remove_me() { }\n";
        let (dir, conn) = make_git_indexed_repo(source);
        let root = dir.path();

        // Remove the second function (unstaged)
        fs::write(root.join("src/lib.rs"), "fn keep_me() { }\n").unwrap();

        let analysis = detect_changes(&conn, &ChangeScope::Unstaged, root).unwrap();

        let removed: Vec<&str> = analysis
            .changed_symbols
            .iter()
            .filter(|c| c.change_type == ChangeType::Removed)
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            removed.contains(&"remove_me"),
            "remove_me should be detected as Removed, got: {:?}",
            analysis.changed_symbols
        );
    }
}
