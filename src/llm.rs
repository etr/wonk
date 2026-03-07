//! LLM description generation and caching for `wonk summary --semantic`.
//!
//! Provides:
//! - Content hash computation from indexed (symbol.id, file.hash) pairs
//! - Prompt construction from structural metrics
//! - Ollama `/api/generate` client (sync, ureq-based)
//! - SQLite cache layer for generated descriptions

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use ureq::Agent;

use crate::config::LlmConfig;
use crate::errors::LlmError;
use crate::types::{SummaryMetrics, SummaryPathType};

// ---------------------------------------------------------------------------
// Query helper
// ---------------------------------------------------------------------------

/// Execute a prepared SQL query and collect results into a Vec,
/// mapping all rusqlite errors to `LlmError::QueryFailed`.
fn query_vec<T>(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::types::ToSql],
    mapper: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> std::result::Result<Vec<T>, LlmError> {
    let mut stmt = conn
        .prepare_cached(sql)
        .map_err(|e| LlmError::QueryFailed(e.to_string()))?;
    stmt.query_map(params, mapper)
        .map_err(|e| LlmError::QueryFailed(e.to_string()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| LlmError::QueryFailed(e.to_string()))
}

// ---------------------------------------------------------------------------
// Content hash computation (DR-019)
// ---------------------------------------------------------------------------

/// Compute a content hash for a path based on the sorted `(symbol.id, file.hash)`
/// pairs under it.  Used for cache invalidation: if any file changes, its hash
/// changes, and the overall content hash changes.
pub fn compute_content_hash(
    conn: &Connection,
    like_pattern: &str,
    path_type: SummaryPathType,
) -> std::result::Result<String, LlmError> {
    let query = match path_type {
        SummaryPathType::File => {
            "SELECT s.id, f.hash FROM symbols s \
             JOIN files f ON s.file = f.path \
             WHERE f.path = ?1 \
             ORDER BY s.id"
        }
        SummaryPathType::Directory => {
            "SELECT s.id, f.hash FROM symbols s \
             JOIN files f ON s.file = f.path \
             WHERE f.path LIKE ?1 ESCAPE '\\' \
             ORDER BY s.id"
        }
    };

    let rows = query_vec(conn, query, &[&like_pattern], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut hasher = Sha256::new();
    for (id, hash) in &rows {
        hasher.update(id.to_le_bytes());
        hasher.update(hash.as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// ---------------------------------------------------------------------------
// Prompt construction (PRD-SUM-REQ-010)
// ---------------------------------------------------------------------------

/// Maximum number of symbol signatures to include in the prompt.
const MAX_PROMPT_SIGNATURES: usize = 100;

/// Strip newlines and control characters from code-derived strings
/// to prevent prompt injection via crafted source code.
fn sanitize(s: &str) -> String {
    s.replace(['\n', '\r', '\0'], " ")
}

/// Build a prompt for the LLM to generate a 2-3 sentence description.
pub fn build_prompt(
    conn: &Connection,
    path: &str,
    like_pattern: &str,
    path_type: SummaryPathType,
    metrics: &SummaryMetrics,
) -> std::result::Result<String, LlmError> {
    let mut prompt = String::with_capacity(2048);

    writeln!(
        prompt,
        "Describe the following code path: `{}`\n",
        sanitize(path)
    )
    .unwrap();

    // Language breakdown.
    if !metrics.language_breakdown.is_empty() {
        prompt.push_str("Languages:\n");
        for (lang, count) in &metrics.language_breakdown {
            writeln!(prompt, "- {lang}: {count} files").unwrap();
        }
        prompt.push('\n');
    }

    // Symbol signatures by kind.
    let sig_query = match path_type {
        SummaryPathType::File => {
            "SELECT kind, name, signature FROM symbols \
             WHERE file = ?1 \
             ORDER BY kind, name LIMIT ?2"
        }
        SummaryPathType::Directory => {
            "SELECT kind, name, signature FROM symbols \
             WHERE file LIKE ?1 ESCAPE '\\' \
             ORDER BY kind, name LIMIT ?2"
        }
    };

    let limit = MAX_PROMPT_SIGNATURES as i64;
    let sigs = query_vec(
        conn,
        sig_query,
        &[&like_pattern as &dyn rusqlite::types::ToSql, &limit],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        },
    )?;

    if !sigs.is_empty() {
        prompt.push_str("Symbol signatures:\n");
        let mut current_kind = "";
        for (kind, name, sig) in &sigs {
            if kind.as_str() != current_kind {
                current_kind = kind;
                writeln!(prompt, "\n  [{kind}]").unwrap();
            }
            if let Some(s) = sig {
                writeln!(prompt, "  - {}", sanitize(s)).unwrap();
            } else {
                writeln!(prompt, "  - {}", sanitize(name)).unwrap();
            }
        }
        prompt.push('\n');
    }

    // Import/export relationships.
    let import_query = match path_type {
        SummaryPathType::File => {
            "SELECT DISTINCT import_path FROM file_imports WHERE source_file = ?1 LIMIT 50"
        }
        SummaryPathType::Directory => {
            "SELECT DISTINCT import_path FROM file_imports WHERE source_file LIKE ?1 ESCAPE '\\' LIMIT 50"
        }
    };

    let imports = query_vec(conn, import_query, &[&like_pattern], |row| {
        row.get::<_, String>(0)
    })?;

    if !imports.is_empty() {
        prompt.push_str("Dependencies:\n");
        for imp in &imports {
            writeln!(prompt, "- {}", sanitize(imp)).unwrap();
        }
        prompt.push('\n');
    }

    prompt.push_str(
        "Based on the code structure above, write a concise 2-3 sentence description \
         of what this code module/file does. Focus on its purpose and key responsibilities. \
         Do not include any code. Do not list individual functions.",
    );

    Ok(prompt)
}

// ---------------------------------------------------------------------------
// Directory overview prompt (for directories with children)
// ---------------------------------------------------------------------------

/// Build a prompt for an LLM to generate a directory overview from children.
pub fn build_directory_overview_prompt(
    path: &str,
    children: &[crate::types::SummaryResult],
) -> String {
    use std::fmt::Write as _;

    let mut prompt = String::with_capacity(2048);

    writeln!(
        prompt,
        "Summarize the following code directory: `{}`\n",
        sanitize(path)
    )
    .unwrap();

    prompt.push_str("Files:\n");
    for child in children {
        let kind = if child.path_type == crate::types::SummaryPathType::Directory {
            "dir"
        } else {
            "file"
        };
        writeln!(
            prompt,
            "- {} ({}, {} lines)",
            sanitize(&child.path),
            kind,
            child.metrics.line_count
        )
        .unwrap();

        // Key symbols (max 10 per file)
        for sym in child.symbols.iter().take(10) {
            writeln!(prompt, "    {} {}", sym.kind, sanitize(&sym.name)).unwrap();
        }
    }

    if children.iter().any(|c| !c.import_edges.is_empty()) {
        prompt.push_str("\nImport relationships:\n");
        for child in children {
            for edge in &child.import_edges {
                writeln!(
                    prompt,
                    "- {} -> {}",
                    sanitize(&edge.from),
                    sanitize(&edge.to)
                )
                .unwrap();
            }
        }
    }

    prompt.push_str(
        "\nSummarize the modules in this directory and explain how they relate. \
         Write a concise 2-4 sentence overview.",
    );

    prompt
}

// ---------------------------------------------------------------------------
// Ollama /api/generate client
// ---------------------------------------------------------------------------

/// Request body for Ollama `/api/generate`.
#[derive(serde::Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
}

/// Response body from Ollama `/api/generate` (stream=false).
#[derive(serde::Deserialize)]
struct GenerateResponse {
    response: String,
}

/// Call Ollama's `/api/generate` endpoint to produce a text completion.
pub fn generate(config: &LlmConfig, prompt: &str) -> std::result::Result<String, LlmError> {
    // Validate URL scheme to prevent SSRF via crafted config.
    if !config.generate_url.starts_with("http://") && !config.generate_url.starts_with("https://") {
        return Err(LlmError::OllamaError(
            "invalid generate_url: only http:// and https:// schemes are allowed".to_string(),
        ));
    }

    // AgentBuilder::build() returns an AgentConfig; into() converts to Agent.
    let agent: Agent = Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .timeout_global(Some(std::time::Duration::from_secs(120)))
        .http_status_as_error(false)
        .build()
        .into();

    let req_body = GenerateRequest {
        model: &config.model,
        prompt,
        stream: false,
    };

    let response = match agent.post(&config.generate_url).send_json(&req_body) {
        Ok(resp) => resp,
        Err(_) => return Err(LlmError::OllamaUnreachable),
    };

    let status = response.status();

    if status == 404 {
        return Err(LlmError::ModelNotFound(config.model.clone()));
    }

    if status != 200 {
        let raw = response
            .into_body()
            .read_to_string()
            .unwrap_or_else(|_| "unknown error".to_string());
        // Truncate at char boundary to avoid panic on multi-byte UTF-8.
        let body: String = raw.chars().take(512).collect();
        return Err(LlmError::OllamaError(format!("HTTP {status}: {body}")));
    }

    let body_str = response
        .into_body()
        .read_to_string()
        .map_err(|_| LlmError::InvalidResponse)?;

    let gen_response: GenerateResponse =
        serde_json::from_str(&body_str).map_err(|_| LlmError::InvalidResponse)?;

    Ok(gen_response.response.trim().to_string())
}

// ---------------------------------------------------------------------------
// Cache layer
// ---------------------------------------------------------------------------

/// Check for a cached description matching the given path and content hash.
pub fn get_cached(conn: &Connection, path: &str, content_hash: &str) -> Option<String> {
    conn.query_row(
        "SELECT description FROM summaries WHERE path = ?1 AND content_hash = ?2",
        rusqlite::params![path, content_hash],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Store (or replace) a cached description for the given path.
pub fn store_cache(
    conn: &Connection,
    path: &str,
    content_hash: &str,
    description: &str,
) -> std::result::Result<(), LlmError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    conn.execute(
        "INSERT OR REPLACE INTO summaries (path, content_hash, description, created_at) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![path, content_hash, description, now],
    )
    .map_err(|e| LlmError::QueryFailed(format!("cache write failed: {e}")))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::pipeline;
    use std::fs;
    use tempfile::TempDir;

    /// Create a minimal indexed repo and return (TempDir, Connection).
    fn make_indexed_repo(files: &[(&str, &str)]) -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();

        for (path, content) in files {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&full, content).unwrap();
        }

        pipeline::build_index(root, true).unwrap();
        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        db::ensure_summaries_table(&conn).unwrap();
        (dir, conn)
    }

    // -- Content hash tests ---------------------------------------------------

    #[test]
    fn content_hash_deterministic() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\nfn world() {}\n")]);

        let h1 = compute_content_hash(&conn, "src/lib.rs", SummaryPathType::File).unwrap();
        let h2 = compute_content_hash(&conn, "src/lib.rs", SummaryPathType::File).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex
    }

    #[test]
    fn content_hash_directory() {
        let (_dir, conn) = make_indexed_repo(&[
            ("src/a.rs", "fn alpha() {}\n"),
            ("src/b.rs", "fn beta() {}\n"),
        ]);

        let h = compute_content_hash(&conn, "src/%", SummaryPathType::Directory).unwrap();
        assert_eq!(h.len(), 64);
    }

    #[test]
    fn content_hash_empty_path() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);

        // Nonexistent path yields a valid hash (of empty input).
        let h = compute_content_hash(&conn, "nonexistent/%", SummaryPathType::Directory).unwrap();
        assert_eq!(h.len(), 64);
    }

    // -- Prompt construction tests -------------------------------------------

    #[test]
    fn build_prompt_includes_path() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\nfn world() {}\n")]);

        let metrics = SummaryMetrics {
            file_count: 1,
            line_count: 2,
            symbol_counts: vec![("function".into(), 2)],
            language_breakdown: vec![("Rust".into(), 1)],
            dependency_count: 0,
        };

        let prompt = build_prompt(
            &conn,
            "src/lib.rs",
            "src/lib.rs",
            SummaryPathType::File,
            &metrics,
        )
        .unwrap();

        assert!(prompt.contains("src/lib.rs"));
        assert!(prompt.contains("Rust"));
        assert!(prompt.contains("2-3 sentence"));
    }

    #[test]
    fn build_prompt_includes_signatures() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\nfn world() {}\n")]);

        let metrics = SummaryMetrics {
            file_count: 1,
            line_count: 2,
            symbol_counts: vec![("function".into(), 2)],
            language_breakdown: vec![("Rust".into(), 1)],
            dependency_count: 0,
        };

        let prompt = build_prompt(
            &conn,
            "src/lib.rs",
            "src/lib.rs",
            SummaryPathType::File,
            &metrics,
        )
        .unwrap();

        assert!(prompt.contains("Symbol signatures:"));
        // Should have at least one function reference.
        assert!(prompt.contains("hello") || prompt.contains("world"));
    }

    #[test]
    fn build_prompt_directory_with_deps() {
        let js_source = "import { foo } from './bar';\nfunction main() {}\n";
        let (_dir, conn) = make_indexed_repo(&[
            ("src/app.js", js_source),
            ("src/bar.js", "export function foo() {}\n"),
        ]);

        let metrics = SummaryMetrics {
            file_count: 2,
            line_count: 4,
            symbol_counts: vec![("function".into(), 2)],
            language_breakdown: vec![("JavaScript".into(), 2)],
            dependency_count: 1,
        };

        let prompt =
            build_prompt(&conn, "src/", "src/%", SummaryPathType::Directory, &metrics).unwrap();

        assert!(prompt.contains("Dependencies:"));
    }

    #[test]
    fn sanitize_strips_newlines() {
        assert_eq!(sanitize("fn foo()\nbar"), "fn foo() bar");
        assert_eq!(sanitize("clean"), "clean");
        assert_eq!(sanitize("a\r\nb\0c"), "a  b c");
    }

    // -- Cache tests ---------------------------------------------------------

    #[test]
    fn cache_miss_returns_none() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);
        assert!(get_cached(&conn, "src/", "somehash").is_none());
    }

    #[test]
    fn cache_hit_returns_description() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);
        store_cache(&conn, "src/", "abc123", "A routing module.").unwrap();
        let desc = get_cached(&conn, "src/", "abc123");
        assert_eq!(desc, Some("A routing module.".to_string()));
    }

    #[test]
    fn cache_invalidated_by_hash_change() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);
        store_cache(&conn, "src/", "hash_old", "Old description.").unwrap();

        // Different hash = cache miss.
        assert!(get_cached(&conn, "src/", "hash_new").is_none());
    }

    #[test]
    fn cache_upsert_on_same_path() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn hello() {}\n")]);
        store_cache(&conn, "src/", "hash1", "First description.").unwrap();
        store_cache(&conn, "src/", "hash2", "Second description.").unwrap();

        // Path is PRIMARY KEY -- only one row.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM summaries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Should return the new description with the new hash.
        assert_eq!(
            get_cached(&conn, "src/", "hash2"),
            Some("Second description.".to_string())
        );
        assert!(get_cached(&conn, "src/", "hash1").is_none());
    }

    // -- Generate client tests -----------------------------------------------

    #[test]
    fn generate_unreachable_returns_error() {
        let config = LlmConfig {
            model: "llama3.2:3b".to_string(),
            generate_url: "http://127.0.0.1:19999/api/generate".to_string(),
        };

        let result = generate(&config, "test prompt");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), LlmError::OllamaUnreachable));
    }

    #[test]
    fn generate_rejects_invalid_url_scheme() {
        let config = LlmConfig {
            model: "llama3.2:3b".to_string(),
            generate_url: "file:///etc/passwd".to_string(),
        };

        let result = generate(&config, "test prompt");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), LlmError::OllamaError(_)));
    }
}
