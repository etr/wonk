//! Database layer for SQLite storage.
//!
//! Provides connection management, schema creation (including FTS5 content-sync),
//! repo root discovery, and index path computation.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Schema SQL
// ---------------------------------------------------------------------------

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    file TEXT NOT NULL,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    end_line INTEGER,
    scope TEXT,
    signature TEXT,
    language TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS "references" (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    file TEXT NOT NULL,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    context TEXT,
    caller_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL,
    confidence REAL DEFAULT 0.5
);

CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    language TEXT,
    hash TEXT NOT NULL,
    last_indexed INTEGER NOT NULL,
    line_count INTEGER,
    symbols_count INTEGER
);

CREATE TABLE IF NOT EXISTS daemon_status (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Indexes
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file);
CREATE INDEX IF NOT EXISTS idx_symbols_kind ON symbols(kind);
CREATE INDEX IF NOT EXISTS idx_references_name ON "references"(name);
CREATE INDEX IF NOT EXISTS idx_references_file ON "references"(file);

-- File-level import tracking for dependency graph
CREATE TABLE IF NOT EXISTS file_imports (
    id INTEGER PRIMARY KEY,
    source_file TEXT NOT NULL,
    import_path TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_file_imports_source ON file_imports(source_file);
CREATE INDEX IF NOT EXISTS idx_file_imports_target ON file_imports(import_path);
"#;

// Table populated by TASK-066 (inheritance extraction) and TASK-067 (pipeline wiring).
const TYPE_EDGES_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS type_edges (
    id INTEGER PRIMARY KEY,
    child_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    parent_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    relationship TEXT NOT NULL,
    UNIQUE(child_id, parent_id, relationship)
);
CREATE INDEX IF NOT EXISTS idx_type_edges_child ON type_edges(child_id);
CREATE INDEX IF NOT EXISTS idx_type_edges_parent ON type_edges(parent_id);
"#;

const EMBEDDINGS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS embeddings (
    id INTEGER PRIMARY KEY,
    symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    file TEXT NOT NULL,
    chunk_text TEXT NOT NULL,
    vector BLOB NOT NULL,
    stale INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    UNIQUE(symbol_id)
);
CREATE INDEX IF NOT EXISTS idx_embeddings_file ON embeddings(file);
"#;

const SUMMARIES_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS summaries (
    path TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    description TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
"#;

const FTS_SQL: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS symbols_fts USING fts5(
    name, kind, file, content=symbols, content_rowid=id
);
"#;

const TRIGGERS_SQL: &str = r#"
CREATE TRIGGER IF NOT EXISTS symbols_ai AFTER INSERT ON symbols BEGIN
    INSERT INTO symbols_fts(rowid, name, kind, file)
    VALUES (new.id, new.name, new.kind, new.file);
END;

CREATE TRIGGER IF NOT EXISTS symbols_bd BEFORE DELETE ON symbols BEGIN
    INSERT INTO symbols_fts(symbols_fts, rowid, name, kind, file)
    VALUES ('delete', old.id, old.name, old.kind, old.file);
END;

CREATE TRIGGER IF NOT EXISTS symbols_bu BEFORE UPDATE ON symbols BEGIN
    INSERT INTO symbols_fts(symbols_fts, rowid, name, kind, file)
    VALUES ('delete', old.id, old.name, old.kind, old.file);
END;

CREATE TRIGGER IF NOT EXISTS symbols_au AFTER UPDATE ON symbols BEGIN
    INSERT INTO symbols_fts(rowid, name, kind, file)
    VALUES (new.id, new.name, new.kind, new.file);
END;
"#;

// ---------------------------------------------------------------------------
// Connection management
// ---------------------------------------------------------------------------

/// Open (or create) a SQLite database at `path`, apply the full schema, and
/// set pragmas suitable for concurrent access.
pub fn open(path: &Path) -> Result<Connection> {
    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating index directory {}", parent.display()))?;
    }

    let conn =
        Connection::open(path).with_context(|| format!("opening database {}", path.display()))?;

    apply_pragmas(&conn)?;
    apply_schema(&conn)?;

    Ok(conn)
}

/// Open an **existing** database without running schema creation.  Useful when
/// you only need to read and you know the DB already exists.
pub fn open_existing(path: &Path) -> Result<Connection> {
    if !path.exists() {
        bail!("index not found at {}", path.display());
    }
    let conn =
        Connection::open(path).with_context(|| format!("opening database {}", path.display()))?;

    apply_pragmas(&conn)?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )
    .context("setting database pragmas")?;
    Ok(())
}

fn apply_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)
        .context("creating base tables and indexes")?;
    // Column migrations must run before any SQL that references these columns.
    ensure_caller_id_column(conn)?;
    ensure_confidence_column(conn)?;
    ensure_doc_comment_column(conn)?;
    conn.execute_batch(TYPE_EDGES_SQL)
        .context("creating type_edges table")?;
    conn.execute_batch(EMBEDDINGS_SQL)
        .context("creating embeddings table")?;
    conn.execute_batch(SUMMARIES_SQL)
        .context("creating summaries table")?;
    conn.execute_batch(FTS_SQL)
        .context("creating FTS5 virtual table")?;
    conn.execute_batch(TRIGGERS_SQL)
        .context("creating FTS5 sync triggers")?;
    Ok(())
}

/// Ensure the `embeddings` table exists, creating it if missing.
///
/// This handles schema migration for V1 indexes that were created before
/// embedding support was added.  Safe to call on databases that already
/// have the table (uses `CREATE TABLE IF NOT EXISTS`).
pub fn ensure_embeddings_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(EMBEDDINGS_SQL)
        .context("creating embeddings table (migration)")?;
    Ok(())
}

/// Ensure the `summaries` table exists, creating it if missing.
///
/// This handles schema migration for indexes created before LLM description
/// caching was added.  Safe to call on databases that already have the table
/// (uses `CREATE TABLE IF NOT EXISTS`).
pub fn ensure_summaries_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(SUMMARIES_SQL)
        .context("creating summaries table (migration)")?;
    Ok(())
}

/// Ensure the `confidence` column exists on the `references` table.
///
/// Handles schema migration for pre-V4 indexes that lack the confidence
/// scoring column.  Uses `PRAGMA table_info` to check before altering.
pub fn ensure_confidence_column(conn: &Connection) -> Result<()> {
    let has_column: bool = conn
        .prepare("PRAGMA table_info(\"references\")")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == "confidence");

    if !has_column {
        conn.execute_batch("ALTER TABLE \"references\" ADD COLUMN confidence REAL DEFAULT 0.5;")
            .context("adding confidence column to references table")?;
    }

    // Always run CREATE INDEX IF NOT EXISTS for idempotent migration.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_references_name_confidence ON \"references\"(name, confidence);
         CREATE INDEX IF NOT EXISTS idx_references_caller_confidence ON \"references\"(caller_id, confidence);",
    )
    .context("creating confidence indexes")?;

    Ok(())
}

/// Ensure the `type_edges` table exists, creating it if missing.
///
/// Handles schema migration for indexes created before type hierarchy
/// support was added.  Safe to call on databases that already have the
/// table (uses `CREATE TABLE IF NOT EXISTS`).
pub fn ensure_type_edges_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(TYPE_EDGES_SQL)
        .context("creating type_edges table (migration)")?;
    Ok(())
}

/// Ensure the `doc_comment` column exists on the `symbols` table.
///
/// Handles schema migration for indexes created before doc comment
/// extraction was added.
pub fn ensure_doc_comment_column(conn: &Connection) -> Result<()> {
    let has_column: bool = conn
        .prepare("PRAGMA table_info(symbols)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == "doc_comment");

    if !has_column {
        conn.execute_batch("ALTER TABLE symbols ADD COLUMN doc_comment TEXT;")
            .context("adding doc_comment column to symbols table")?;
    }

    Ok(())
}

/// Ensure the `caller_id` column exists on the `references` table.
///
/// Handles schema migration for pre-V3 indexes that lack the call graph
/// FK.  Uses `PRAGMA table_info` to check before altering.
pub fn ensure_caller_id_column(conn: &Connection) -> Result<()> {
    let has_column: bool = conn
        .prepare("PRAGMA table_info(\"references\")")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == "caller_id");

    if !has_column {
        conn.execute_batch(
            "ALTER TABLE \"references\" ADD COLUMN caller_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL;",
        )
        .context("adding caller_id column to references table")?;
    }

    // Always run CREATE INDEX IF NOT EXISTS for idempotent migration.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_references_caller ON \"references\"(caller_id);",
    )
    .context("creating caller_id index")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Repo root discovery
// ---------------------------------------------------------------------------

/// Walk upwards from `start` looking for a `.git` directory or `.wonk`
/// directory.  Returns the directory that contains the marker.
pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let mut current = start.to_path_buf();
    // Canonicalize so we don't get stuck in symlink loops, but tolerate
    // failure (e.g. non-existent trailing component).
    if let Ok(canon) = fs::canonicalize(&current) {
        current = canon;
    }
    loop {
        if current.join(".git").exists() || current.join(".wonk").exists() {
            return Ok(current);
        }
        if !current.pop() {
            bail!(
                "could not find repository root (no .git or .wonk) starting from {}",
                start.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Index path computation
// ---------------------------------------------------------------------------

/// Compute the SHA-256-short hash (first 16 hex chars) of `repo_path`.
pub fn repo_hash(repo_path: &Path) -> String {
    let canonical = repo_path.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    hex_encode_short(&digest)
}

/// First 16 hex characters of a byte slice.
fn hex_encode_short(bytes: &[u8]) -> String {
    // 16 hex chars = 8 bytes
    bytes.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Where the index lives when using the **central** (default) location:
/// `~/.wonk/repos/<hash>/index.db`
pub fn central_index_path(repo_path: &Path) -> Result<PathBuf> {
    let home = home_dir()?;
    let hash = repo_hash(repo_path);
    Ok(home.join(".wonk").join("repos").join(hash).join("index.db"))
}

/// Where the index lives when using the **local** location:
/// `<repo_root>/.wonk/index.db`
pub fn local_index_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".wonk").join("index.db")
}

/// Resolve the index path for a given repo, respecting `local` flag.
pub fn index_path_for(repo_root: &Path, local: bool) -> Result<PathBuf> {
    if local {
        Ok(local_index_path(repo_root))
    } else {
        central_index_path(repo_root)
    }
}

/// Check whether an index exists for the given repo root.
///
/// Checks the local path first (`.wonk/index.db`), then the central path
/// (`~/.wonk/repos/<hash>/index.db`).  Returns the path if found.
pub fn find_existing_index(repo_root: &Path) -> Option<PathBuf> {
    let local = local_index_path(repo_root);
    if local.exists() {
        return Some(local);
    }
    if let Ok(central) = central_index_path(repo_root)
        && central.exists()
    {
        return Some(central);
    }
    None
}

// ---------------------------------------------------------------------------
// meta.json
// ---------------------------------------------------------------------------

/// Metadata written alongside `index.db`.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct Meta {
    pub repo_path: String,
    pub created: u64,
    pub languages: Vec<String>,
    #[serde(default)]
    pub wonk_version: Option<String>,
}

/// Write `meta.json` next to the given `index_db_path`.
pub fn write_meta(index_db_path: &Path, repo_path: &Path, languages: &[String]) -> Result<()> {
    let meta_path = index_db_path
        .parent()
        .expect("index.db must have a parent directory")
        .join("meta.json");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let meta = Meta {
        repo_path: repo_path.to_string_lossy().into_owned(),
        created: now,
        languages: languages.to_vec(),
        wonk_version: Some(env!("CARGO_PKG_VERSION").to_string()),
    };

    let json = serde_json::to_string_pretty(&meta).context("serializing meta.json")?;
    fs::write(&meta_path, json).with_context(|| format!("writing {}", meta_path.display()))?;
    Ok(())
}

/// Read `meta.json` from next to the given `index_db_path`.
pub fn read_meta(index_db_path: &Path) -> Result<Meta> {
    let meta_path = index_db_path
        .parent()
        .expect("index.db must have a parent directory")
        .join("meta.json");

    let data = fs::read_to_string(&meta_path)
        .with_context(|| format!("reading {}", meta_path.display()))?;
    let meta: Meta = serde_json::from_str(&data).context("parsing meta.json")?;
    Ok(meta)
}

// ---------------------------------------------------------------------------
// Symbol detection
// ---------------------------------------------------------------------------

/// Count symbol names in the FTS5 index matching the given pattern.
///
/// Returns 0 if the query fails (e.g. pattern contains characters that are
/// invalid in FTS5 syntax) or if no symbols match.  Used for symbol detection:
/// a non-zero result means the pattern likely refers to a code symbol and
/// ranked mode should be used.
pub fn count_matching_symbols(conn: &Connection, pattern: &str) -> u64 {
    if pattern.is_empty() {
        return 0;
    }
    // Wrap in double quotes to treat as a literal phrase in FTS5.
    // Escape any embedded double quotes by doubling them.
    let escaped = pattern.replace('"', "\"\"");
    let fts_query = format!("\"{}\"", escaped);

    conn.query_row(
        "SELECT COUNT(*) FROM symbols_fts WHERE name MATCH ?1",
        [&fts_query],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0) as u64
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check whether a file path exists in the `files` table.
pub fn file_exists_in_index(conn: &Connection, path: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files WHERE path = ?1",
        [path],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn home_dir() -> Result<PathBuf> {
    // Try $HOME first.  We avoid the `dirs` crate to keep dependencies small.
    if let Ok(home) = std::env::var("HOME") {
        return Ok(PathBuf::from(home));
    }
    bail!("could not determine home directory ($HOME is not set)");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_open_creates_schema() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Check that all expected tables exist.
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"symbols".to_string()));
        assert!(tables.contains(&"references".to_string()));
        assert!(tables.contains(&"files".to_string()));
        assert!(tables.contains(&"daemon_status".to_string()));
        assert!(tables.contains(&"symbols_fts".to_string()));
        assert!(tables.contains(&"file_imports".to_string()));
        assert!(tables.contains(&"embeddings".to_string()));
        assert!(tables.contains(&"type_edges".to_string()));
    }

    #[test]
    fn test_file_imports_table_insert_and_query() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/main.ts", "./utils"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/main.ts", "./config"],
        )
        .unwrap();

        // Query forward deps.
        let imports: Vec<String> = conn
            .prepare("SELECT DISTINCT import_path FROM file_imports WHERE source_file = ?1")
            .unwrap()
            .query_map(rusqlite::params!["src/main.ts"], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(imports.len(), 2);

        // Query reverse deps.
        let rdeps: Vec<String> = conn
            .prepare("SELECT DISTINCT source_file FROM file_imports WHERE import_path = ?1")
            .unwrap()
            .query_map(rusqlite::params!["./utils"], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(rdeps.len(), 1);
        assert_eq!(rdeps[0], "src/main.ts");
    }

    #[test]
    fn test_busy_timeout_is_set() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let timeout: i64 = conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
    }

    #[test]
    fn test_fts5_triggers_insert() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["my_func", "function", "src/main.rs", 10, 0, "rust"],
        )
        .unwrap();

        // FTS should contain the inserted row.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'my_func'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_fts5_triggers_delete() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["del_func", "function", "src/lib.rs", 5, 0, "rust"],
        )
        .unwrap();

        // Verify it's in FTS.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'del_func'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Delete the symbol row — trigger should remove from FTS.
        conn.execute("DELETE FROM symbols WHERE name = 'del_func'", [])
            .unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'del_func'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_fts5_triggers_update() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["old_name", "function", "src/main.rs", 1, 0, "rust"],
        )
        .unwrap();

        // Update the name.
        conn.execute(
            "UPDATE symbols SET name = 'new_name' WHERE name = 'old_name'",
            [],
        )
        .unwrap();

        // Old name should be gone from FTS.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'old_name'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);

        // New name should be present.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'new_name'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_find_repo_root_git() {
        let dir = TempDir::new().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir(&git_dir).unwrap();
        let sub = dir.path().join("a").join("b").join("c");
        fs::create_dir_all(&sub).unwrap();

        let root = find_repo_root(&sub).unwrap();
        assert_eq!(root, fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn test_find_repo_root_wonk() {
        let dir = TempDir::new().unwrap();
        let wonk_dir = dir.path().join(".wonk");
        fs::create_dir(&wonk_dir).unwrap();
        let sub = dir.path().join("x");
        fs::create_dir(&sub).unwrap();

        let root = find_repo_root(&sub).unwrap();
        assert_eq!(root, fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn test_find_repo_root_fails() {
        // Use a tmpdir with no markers at all.
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("lonely");
        fs::create_dir(&sub).unwrap();

        let result = find_repo_root(&sub);
        assert!(result.is_err());
    }

    #[test]
    fn test_repo_hash_deterministic() {
        let path = Path::new("/home/user/projects/myrepo");
        let h1 = repo_hash(path);
        let h2 = repo_hash(path);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
        // Ensure it's all hex characters.
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_central_index_path() {
        let repo = Path::new("/home/user/projects/myrepo");
        let path = central_index_path(repo).unwrap();
        let hash = repo_hash(repo);
        assert!(path.to_string_lossy().contains(&hash));
        assert!(path.to_string_lossy().ends_with("index.db"));
        assert!(path.to_string_lossy().contains(".wonk/repos/"));
    }

    #[test]
    fn test_local_index_path() {
        let repo = Path::new("/home/user/projects/myrepo");
        let path = local_index_path(repo);
        assert_eq!(
            path,
            PathBuf::from("/home/user/projects/myrepo/.wonk/index.db")
        );
    }

    #[test]
    fn test_write_and_read_meta() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let repo_path = Path::new("/fake/repo");
        let langs = vec!["rust".to_string(), "python".to_string()];

        write_meta(&db_path, repo_path, &langs).unwrap();

        let meta = read_meta(&db_path).unwrap();
        assert_eq!(meta.repo_path, "/fake/repo");
        assert_eq!(meta.languages, vec!["rust", "python"]);
        assert!(meta.created > 0);
    }

    #[test]
    fn test_open_idempotent() {
        // Opening the same database twice should not fail.
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let _conn1 = open(&db_path).unwrap();
        drop(_conn1);
        let _conn2 = open(&db_path).unwrap();
    }

    #[test]
    fn test_schema_references_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["my_func", "src/main.rs", 20, 4, "let x = my_func();"],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM \"references\"", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_schema_files_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO files (path, language, hash, last_indexed, line_count, symbols_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["src/main.rs", "rust", "abc123", 1700000000, 100, 5],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_schema_daemon_status_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO daemon_status (key, value, updated_at) VALUES (?1, ?2, ?3)",
            rusqlite::params!["pid", "12345", 1700000000],
        )
        .unwrap();

        let val: String = conn
            .query_row(
                "SELECT value FROM daemon_status WHERE key = 'pid'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(val, "12345");
    }

    #[test]
    fn test_index_path_for_local() {
        let repo = Path::new("/home/user/repo");
        let path = index_path_for(repo, true).unwrap();
        assert_eq!(path, PathBuf::from("/home/user/repo/.wonk/index.db"));
    }

    #[test]
    fn test_index_path_for_central() {
        let repo = Path::new("/home/user/repo");
        let path = index_path_for(repo, false).unwrap();
        assert!(path.to_string_lossy().contains(".wonk/repos/"));
        assert!(path.to_string_lossy().ends_with("index.db"));
    }

    #[test]
    fn test_find_existing_index_none() {
        let dir = TempDir::new().unwrap();
        assert!(find_existing_index(dir.path()).is_none());
    }

    #[test]
    fn test_find_existing_index_local() {
        let dir = TempDir::new().unwrap();
        let wonk = dir.path().join(".wonk");
        fs::create_dir(&wonk).unwrap();
        let db_path = wonk.join("index.db");
        fs::write(&db_path, b"fake").unwrap();
        assert_eq!(find_existing_index(dir.path()), Some(db_path));
    }

    #[test]
    fn test_find_existing_index_central() {
        let dir = TempDir::new().unwrap();
        // Create a central index path and ensure it exists.
        let central = central_index_path(dir.path()).unwrap();
        fs::create_dir_all(central.parent().unwrap()).unwrap();
        fs::write(&central, b"fake").unwrap();
        assert_eq!(find_existing_index(dir.path()), Some(central));
    }

    #[test]
    fn test_find_existing_index_prefers_local() {
        let dir = TempDir::new().unwrap();
        // Create both local and central.
        let wonk = dir.path().join(".wonk");
        fs::create_dir(&wonk).unwrap();
        let local = wonk.join("index.db");
        fs::write(&local, b"local").unwrap();
        let central = central_index_path(dir.path()).unwrap();
        fs::create_dir_all(central.parent().unwrap()).unwrap();
        fs::write(&central, b"central").unwrap();
        // Local should win.
        assert_eq!(find_existing_index(dir.path()), Some(local));
    }

    // -- count_matching_symbols tests ----------------------------------------

    #[test]
    fn test_count_matching_symbols_found() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES ('processPayment', 'function', 'pay.rs', 1, 0, 'rust')",
            [],
        ).unwrap();
        assert_eq!(count_matching_symbols(&conn, "processPayment"), 1);
    }

    #[test]
    fn test_count_matching_symbols_none() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();
        assert_eq!(count_matching_symbols(&conn, "processPayment"), 0);
    }

    #[test]
    fn test_count_matching_symbols_multiple() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES ('process', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES ('process', 'function', 'b.rs', 5, 0, 'rust')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES ('process', 'method', 'c.rs', 10, 0, 'rust')",
            [],
        ).unwrap();
        assert_eq!(count_matching_symbols(&conn, "process"), 3);
    }

    #[test]
    fn test_count_matching_symbols_special_chars() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();
        // "connection refused" is not a valid symbol name; should return 0.
        assert_eq!(count_matching_symbols(&conn, "connection refused"), 0);
    }

    #[test]
    fn test_count_matching_symbols_fts_syntax_safe() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();
        // Patterns with FTS5-special chars should not panic or error.
        assert_eq!(count_matching_symbols(&conn, ""), 0);
        assert_eq!(count_matching_symbols(&conn, "foo OR bar"), 0);
        assert_eq!(count_matching_symbols(&conn, "foo*"), 0);
        assert_eq!(count_matching_symbols(&conn, "\"quoted\""), 0);
    }

    // -- Embeddings table tests ---------------------------------------------

    #[test]
    fn test_open_creates_embeddings_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"embeddings".to_string()));
    }

    #[test]
    fn test_embeddings_table_columns() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Insert a symbol first (FK target).
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'foo', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        ).unwrap();

        // Insert an embedding with all columns.
        conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, stale, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![1, "a.rs", "fn foo() {}", vec![0u8; 16], 0, 1700000000i64],
        ).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_embeddings_unique_symbol_id() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'foo', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        ).unwrap();

        conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, created_at) VALUES (1, 'a.rs', 'text1', X'00', 1000)",
            [],
        ).unwrap();

        // Second insert with same symbol_id should fail (UNIQUE constraint).
        let result = conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, created_at) VALUES (1, 'a.rs', 'text2', X'00', 2000)",
            [],
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_embeddings_cascade_delete() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'bar', 'function', 'b.rs', 1, 0, 'rust')",
            [],
        ).unwrap();

        conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, created_at) VALUES (1, 'b.rs', 'fn bar()', X'AABB', 1000)",
            [],
        ).unwrap();

        // Verify embedding exists.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Delete the symbol -- cascade should remove embedding.
        conn.execute("DELETE FROM symbols WHERE id = 1", [])
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_embeddings_stale_default() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'baz', 'function', 'c.rs', 1, 0, 'rust')",
            [],
        ).unwrap();

        // Insert without specifying stale -- should default to 0.
        conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, created_at) VALUES (1, 'c.rs', 'text', X'00', 1000)",
            [],
        ).unwrap();

        let stale: i64 = conn
            .query_row(
                "SELECT stale FROM embeddings WHERE symbol_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0);
    }

    #[test]
    fn test_embeddings_file_index_exists() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Check that the index exists.
        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name = 'idx_embeddings_file'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(indexes.len(), 1);
    }

    #[test]
    fn test_ensure_embeddings_table_on_v1_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Simulate a V1 database: apply only base schema without embeddings.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.execute_batch(FTS_SQL).unwrap();
        conn.execute_batch(TRIGGERS_SQL).unwrap();

        // Embeddings table should NOT exist yet (V1 schema).
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='embeddings'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.is_empty());

        // Migrate: ensure_embeddings_table should create it.
        ensure_embeddings_table(&conn).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='embeddings'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(tables.len(), 1);
    }

    #[test]
    fn test_ensure_embeddings_table_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Table already exists via open(). Calling ensure_embeddings_table
        // again should not fail.
        ensure_embeddings_table(&conn).unwrap();
        ensure_embeddings_table(&conn).unwrap();
    }

    // -- file_exists_in_index tests ------------------------------------------

    #[test]
    fn test_file_exists_in_index_found() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO files (path, language, hash, last_indexed) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["src/main.ts", "TypeScript", "abc123", 0],
        )
        .unwrap();

        assert!(file_exists_in_index(&conn, "src/main.ts").unwrap());
    }

    #[test]
    fn test_file_exists_in_index_not_found() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        assert!(!file_exists_in_index(&conn, "src/nonexistent.ts").unwrap());
    }

    // -- caller_id column tests -----------------------------------------------

    #[test]
    fn test_new_db_has_caller_id_column() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(columns.contains(&"caller_id".to_string()));
    }

    #[test]
    fn test_caller_id_index_exists() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_references_caller'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(indexes.len(), 1);
    }

    #[test]
    fn test_caller_id_nullable() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Insert without caller_id — should default to NULL.
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["foo", "a.rs", 1, 0, "foo()"],
        )
        .unwrap();

        let caller_id: Option<i64> = conn
            .query_row(
                "SELECT caller_id FROM \"references\" WHERE name = 'foo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(caller_id.is_none());
    }

    #[test]
    fn test_caller_id_with_valid_fk() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'main', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context, caller_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["foo", "a.rs", 5, 4, "foo()", 1i64],
        )
        .unwrap();

        let caller_id: Option<i64> = conn
            .query_row(
                "SELECT caller_id FROM \"references\" WHERE name = 'foo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(caller_id, Some(1));
    }

    #[test]
    fn test_caller_id_on_delete_set_null() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'main', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context, caller_id) VALUES ('foo', 'a.rs', 5, 4, 'foo()', 1)",
            [],
        )
        .unwrap();

        // Delete the symbol — FK should SET NULL.
        conn.execute("DELETE FROM symbols WHERE id = 1", [])
            .unwrap();

        let caller_id: Option<i64> = conn
            .query_row(
                "SELECT caller_id FROM \"references\" WHERE name = 'foo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(caller_id.is_none());
    }

    #[test]
    fn test_ensure_caller_id_migration_on_v2_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Simulate a V2 database: base schema + embeddings but no caller_id.
        // Use the old schema SQL without caller_id.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS symbols (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                file TEXT NOT NULL,
                line INTEGER NOT NULL,
                col INTEGER NOT NULL,
                end_line INTEGER,
                scope TEXT,
                signature TEXT,
                language TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS "references" (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                file TEXT NOT NULL,
                line INTEGER NOT NULL,
                col INTEGER NOT NULL,
                context TEXT
            );
            "#,
        )
        .unwrap();

        // caller_id should NOT exist yet.
        let has_caller_id: bool = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .any(|name| name == "caller_id");
        assert!(!has_caller_id);

        // Run migration.
        ensure_caller_id_column(&conn).unwrap();

        let has_caller_id: bool = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .any(|name| name == "caller_id");
        assert!(has_caller_id);
    }

    #[test]
    fn test_ensure_caller_id_column_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Column already exists via open(). Calling again should not fail.
        ensure_caller_id_column(&conn).unwrap();
        ensure_caller_id_column(&conn).unwrap();
    }

    // -- Summaries table tests ------------------------------------------------

    #[test]
    fn test_open_creates_summaries_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='summaries'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(tables.len(), 1);
    }

    #[test]
    fn test_summaries_insert_and_query() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO summaries (path, content_hash, description, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["src/", "abc123", "This module handles routing.", 1700000000i64],
        ).unwrap();

        let desc: String = conn
            .query_row(
                "SELECT description FROM summaries WHERE path = ?1 AND content_hash = ?2",
                rusqlite::params!["src/", "abc123"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(desc, "This module handles routing.");
    }

    #[test]
    fn test_summaries_upsert_on_path() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT OR REPLACE INTO summaries (path, content_hash, description, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["src/", "hash1", "Old description.", 1000],
        ).unwrap();

        conn.execute(
            "INSERT OR REPLACE INTO summaries (path, content_hash, description, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["src/", "hash2", "New description.", 2000],
        ).unwrap();

        // Should have only one row (path is PRIMARY KEY).
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM summaries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let desc: String = conn
            .query_row(
                "SELECT description FROM summaries WHERE path = 'src/'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(desc, "New description.");
    }

    #[test]
    fn test_ensure_summaries_table_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        ensure_summaries_table(&conn).unwrap();
        ensure_summaries_table(&conn).unwrap();
    }

    #[test]
    fn test_ensure_summaries_table_on_old_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Simulate old database without summaries.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();

        // No summaries table yet.
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='summaries'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.is_empty());

        // Migrate.
        ensure_summaries_table(&conn).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='summaries'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(tables.len(), 1);
    }

    // -- confidence column tests -----------------------------------------------

    #[test]
    fn test_new_db_has_confidence_column() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            columns.contains(&"confidence".to_string()),
            "references table should have confidence column"
        );
    }

    #[test]
    fn test_confidence_default_is_0_5() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Insert without specifying confidence -- should default to 0.5.
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["foo", "a.rs", 1, 0, "foo()"],
        )
        .unwrap();

        let confidence: f64 = conn
            .query_row(
                "SELECT confidence FROM \"references\" WHERE name = 'foo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!((confidence - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_confidence_indexes_exist() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_references_%confidence%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(indexes.contains(&"idx_references_name_confidence".to_string()));
        assert!(indexes.contains(&"idx_references_caller_confidence".to_string()));
    }

    #[test]
    fn test_ensure_confidence_column_migration() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Simulate a pre-V4 database without confidence column.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS symbols (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                file TEXT NOT NULL,
                line INTEGER NOT NULL,
                col INTEGER NOT NULL,
                end_line INTEGER,
                scope TEXT,
                signature TEXT,
                language TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS "references" (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                file TEXT NOT NULL,
                line INTEGER NOT NULL,
                col INTEGER NOT NULL,
                context TEXT,
                caller_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL
            );
            "#,
        )
        .unwrap();

        // confidence should NOT exist yet.
        let has_confidence: bool = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .any(|name| name == "confidence");
        assert!(!has_confidence);

        // Run migration.
        ensure_confidence_column(&conn).unwrap();

        let has_confidence: bool = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .any(|name| name == "confidence");
        assert!(has_confidence);
    }

    #[test]
    fn test_ensure_confidence_column_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        ensure_confidence_column(&conn).unwrap();
        ensure_confidence_column(&conn).unwrap();
    }

    // -- type_edges table tests ------------------------------------------------

    #[test]
    fn test_new_db_has_type_edges_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='type_edges'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(tables.len(), 1);
    }

    #[test]
    fn test_type_edges_insert_and_query() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Create parent and child symbols.
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'Animal', 'class', 'a.rs', 1, 0, 'rust')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (2, 'Dog', 'class', 'a.rs', 10, 0, 'rust')",
            [],
        ).unwrap();

        conn.execute(
            "INSERT INTO type_edges (child_id, parent_id, relationship) VALUES (?1, ?2, ?3)",
            rusqlite::params![2, 1, "extends"],
        )
        .unwrap();

        let rel: String = conn
            .query_row(
                "SELECT relationship FROM type_edges WHERE child_id = 2 AND parent_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rel, "extends");
    }

    #[test]
    fn test_type_edges_unique_constraint() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'A', 'class', 'a.rs', 1, 0, 'rust')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (2, 'B', 'class', 'a.rs', 10, 0, 'rust')",
            [],
        ).unwrap();

        conn.execute(
            "INSERT INTO type_edges (child_id, parent_id, relationship) VALUES (2, 1, 'extends')",
            [],
        )
        .unwrap();

        // Duplicate should fail.
        let result = conn.execute(
            "INSERT INTO type_edges (child_id, parent_id, relationship) VALUES (2, 1, 'extends')",
            [],
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_type_edges_cascade_delete() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'A', 'class', 'a.rs', 1, 0, 'rust')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (2, 'B', 'class', 'a.rs', 10, 0, 'rust')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO type_edges (child_id, parent_id, relationship) VALUES (2, 1, 'extends')",
            [],
        )
        .unwrap();

        // Delete child symbol -- cascade should remove edge.
        conn.execute("DELETE FROM symbols WHERE id = 2", [])
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM type_edges", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_type_edges_bidirectional_indexes() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_type_edges_%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(indexes.contains(&"idx_type_edges_child".to_string()));
        assert!(indexes.contains(&"idx_type_edges_parent".to_string()));
    }

    #[test]
    fn test_ensure_type_edges_table_migration() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Simulate old database without type_edges.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();

        // No type_edges table yet.
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='type_edges'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.is_empty());

        // Migrate.
        ensure_type_edges_table(&conn).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='type_edges'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(tables.len(), 1);
    }

    #[test]
    fn test_open_migrates_legacy_db_without_caller_id_or_confidence() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Create a legacy database without caller_id or confidence columns.
        {
            let conn = Connection::open(&db_path).unwrap();
            apply_pragmas(&conn).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS symbols (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    file TEXT NOT NULL,
                    line INTEGER NOT NULL,
                    col INTEGER NOT NULL,
                    end_line INTEGER,
                    scope TEXT,
                    signature TEXT,
                    language TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS "references" (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    file TEXT NOT NULL,
                    line INTEGER NOT NULL,
                    col INTEGER NOT NULL,
                    context TEXT
                );
                CREATE TABLE IF NOT EXISTS files (
                    path TEXT PRIMARY KEY,
                    language TEXT,
                    hash TEXT NOT NULL,
                    last_indexed INTEGER NOT NULL,
                    line_count INTEGER,
                    symbols_count INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
                CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file);
                CREATE INDEX IF NOT EXISTS idx_symbols_kind ON symbols(kind);
                CREATE INDEX IF NOT EXISTS idx_references_name ON "references"(name);
                CREATE INDEX IF NOT EXISTS idx_references_file ON "references"(file);
                "#,
            )
            .unwrap();
        }

        // open() should succeed and migrate the schema.
        let conn = open(&db_path).unwrap();

        // Verify both columns exist.
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(columns.contains(&"caller_id".to_string()));
        assert!(columns.contains(&"confidence".to_string()));

        // Verify all three indexes exist.
        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_references_%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(indexes.contains(&"idx_references_caller".to_string()));
        assert!(indexes.contains(&"idx_references_name_confidence".to_string()));
        assert!(indexes.contains(&"idx_references_caller_confidence".to_string()));
    }

    #[test]
    fn test_ensure_type_edges_table_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        ensure_type_edges_table(&conn).unwrap();
        ensure_type_edges_table(&conn).unwrap();
    }
}
