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

    let conn = Connection::open(path)
        .with_context(|| format!("opening database {}", path.display()))?;

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
    let conn = Connection::open(path)
        .with_context(|| format!("opening database {}", path.display()))?;

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
    conn.execute_batch(FTS_SQL)
        .context("creating FTS5 virtual table")?;
    conn.execute_batch(TRIGGERS_SQL)
        .context("creating FTS5 sync triggers")?;
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
    bytes
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
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
    if let Ok(central) = central_index_path(repo_root) {
        if central.exists() {
            return Some(central);
        }
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

    let data =
        fs::read_to_string(&meta_path).with_context(|| format!("reading {}", meta_path.display()))?;
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
}
