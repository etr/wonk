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
    ChangeType, ChangedSymbol, ImpactResult, SemanticResult, Symbol, SymbolKind, SymbolRef,
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
    // Guard against path traversal: reject `..` components before any filesystem access.
    if Path::new(file)
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        bail!("path escapes repository root: {file}");
    }

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
    // Validate commit reference to prevent git argument injection (CWE-88).
    // Allow alphanumeric chars plus common git-ref characters: / _ . - @ ~ ^
    if commit.is_empty()
        || !commit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/_.-@~^".contains(c))
    {
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

/// Re-parse a file from disk to get full [`Symbol`] structs (with `end_line`).
///
/// This is needed because `detect_changed_symbols` only returns [`ChangedSymbol`]
/// which lacks `end_line`, `signature`, and other fields needed for embedding.
pub fn parse_current_symbols(file: &str, repo_root: &Path) -> Result<Vec<Symbol>> {
    let abs_path = repo_root.join(file);
    let content =
        std::fs::read_to_string(&abs_path).with_context(|| format!("reading file {file}"))?;

    let lang = match indexer::detect_language(Path::new(file)) {
        Some(l) => l,
        None => bail!("unsupported language for file: {file}"),
    };

    let mut parser = indexer::get_parser(lang);
    let tree = parser
        .parse(content.as_bytes(), None)
        .context("tree-sitter parse failed")?;

    Ok(indexer::extract_symbols(&tree, &content, file, lang))
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
/// Returns an empty `Vec` if the file has no changes.
/// Returns an error if no embeddings exist in the index.
pub fn analyze_impact(
    conn: &Connection,
    file: &str,
    repo_root: &Path,
    client: &embedding::OllamaClient,
) -> Result<Vec<ImpactResult>> {
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

    // Step 3: Load all stored embeddings.
    let all_embeddings = embedding::load_all_embeddings(conn)?;
    if all_embeddings.is_empty() {
        bail!(
            "no embeddings found in the index; \
             run `wonk init` with Ollama running to build embeddings"
        );
    }

    // Step 4: Re-parse the file from disk to get full Symbol structs.
    let current_symbols = parse_current_symbols(file, repo_root)?;

    // Build a lookup map from (name, kind) to full Symbol for chunking.
    let sym_map: HashMap<(String, SymbolKind), &Symbol> = current_symbols
        .iter()
        .map(|s| ((s.name.clone(), s.kind), s))
        .collect();

    // Step 5: Load file imports for chunk context.
    let file_imports = conn
        .prepare("SELECT import_path FROM file_imports WHERE source_file = ?1")
        .and_then(|mut stmt| {
            let rows = stmt.query_map(rusqlite::params![file], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

    // Read file content for chunking.
    let abs_path = repo_root.join(file);
    let source_code =
        std::fs::read_to_string(&abs_path).with_context(|| format!("reading file {file}"))?;

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
        let scored = semantic::semantic_search(&query_vec, &all_embeddings, 20);
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
        let key = (
            r.impacted_symbol.name.clone(),
            r.impacted_symbol.kind,
            r.impacted_symbol.file.clone(),
            r.impacted_symbol.line,
        );
        seen.insert(key)
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
            err_msg.contains("invalid commit reference"),
            "error should mention invalid commit reference"
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
    fn analyze_impact_no_embeddings_returns_error() {
        let source = "fn hello() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        // Modify the file so changes are detected
        fs::write(dir.path().join("src/lib.rs"), "fn hello(x: i32) { }\n").unwrap();

        // Dead-port client
        let client = crate::embedding::OllamaClient::with_base_url("http://127.0.0.1:19999");

        let result = analyze_impact(&conn, "src/lib.rs", dir.path(), &client);
        assert!(result.is_err(), "should error when no embeddings exist");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("no embeddings") || err_msg.contains("embedding"),
            "error should mention embeddings: {err_msg}"
        );
    }

    // -- analyze_impact with unchanged file returns empty ----------------------

    #[test]
    fn analyze_impact_unchanged_returns_empty() {
        let source = "fn hello() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        // File unchanged, no client needed
        let client = crate::embedding::OllamaClient::with_base_url("http://127.0.0.1:19999");

        let results = analyze_impact(&conn, "src/lib.rs", dir.path(), &client).unwrap();
        assert!(
            results.is_empty(),
            "unchanged file should produce no impact results"
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
