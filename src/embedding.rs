//! Ollama embedding API client and symbol chunking engine.
//!
//! Provides a synchronous HTTP client for generating text embeddings via
//! Ollama's `/api/embed` endpoint.  Supports health checking, batch
//! embedding, and configurable timeouts.
//!
//! Also provides the chunking pipeline that transforms indexed symbols into
//! context-rich text chunks suitable for embedding by `nomic-embed-text`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use ureq::Agent;

use crate::errors::EmbeddingError;
use crate::types::{Symbol, SymbolKind};

/// Maximum chunk size in bytes.  `nomic-embed-text` supports 8192 tokens;
/// at ~4 bytes/token this gives 32 KB.
pub const MAX_CHUNK_BYTES: usize = 32_768;

/// Default Ollama server URL.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Default embedding model.
pub const DEFAULT_MODEL: &str = "nomic-embed-text";

// ---------------------------------------------------------------------------
// Serde types for the Ollama /api/embed endpoint
// ---------------------------------------------------------------------------

/// Request body for `POST /api/embed`.
#[derive(Serialize)]
pub(crate) struct EmbedRequest {
    pub model: String,
    pub input: Vec<String>,
}

/// Response body from `POST /api/embed`.
#[derive(Deserialize)]
pub(crate) struct EmbedResponse {
    pub embeddings: Vec<Vec<f32>>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Synchronous HTTP client for Ollama's embedding API.
pub struct OllamaClient {
    agent: Agent,
    pub(crate) base_url: String,
    pub(crate) model: String,
}

impl Default for OllamaClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaClient {
    /// Create a client pointing at the default Ollama URL (`localhost:11434`).
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// Create a client with a custom base URL.
    ///
    /// Configures connection timeout (2 s) and body-read timeout (60 s).
    /// Disables `http_status_as_error` so we can inspect non-200 responses
    /// ourselves.
    pub fn with_base_url(base_url: &str) -> Self {
        let config = Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(2)))
            .timeout_recv_body(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .build();
        let agent: Agent = config.into();
        Self {
            agent,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Check whether the Ollama server is reachable.
    ///
    /// Sends `GET /` and returns `true` if the server responds with 200 OK.
    pub fn is_healthy(&self) -> bool {
        let url = format!("{}/", self.base_url);
        match self.agent.get(&url).call() {
            Ok(resp) => resp.status() == 200,
            Err(_) => false,
        }
    }

    /// Generate embeddings for a batch of texts.
    ///
    /// Returns one `Vec<f32>` per input string.  An empty input slice
    /// short-circuits to an empty result without contacting the server.
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let url = format!("{}/api/embed", self.base_url);
        let request_body = EmbedRequest {
            model: self.model.clone(),
            input: texts.to_vec(),
        };

        let response = self
            .agent
            .post(&url)
            .send_json(&request_body)
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if status != 200 {
            let body = response.into_body().read_to_string().unwrap_or_default();
            return Err(EmbeddingError::OllamaError(extract_error_detail(
                status, &body,
            )));
        }

        let embed_resp: EmbedResponse = response
            .into_body()
            .read_json()
            .map_err(|_| EmbeddingError::InvalidResponse)?;

        Ok(embed_resp.embeddings)
    }

    /// Generate an embedding for a single text.
    ///
    /// Convenience wrapper around [`embed_batch`](Self::embed_batch).
    pub fn embed_single(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let mut results = self.embed_batch(&[text.to_string()])?;
        results.pop().ok_or(EmbeddingError::InvalidResponse)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a ureq transport error to the appropriate [`EmbeddingError`].
///
/// Connection-level failures (refused, host not found, timeout) become
/// [`EmbeddingError::OllamaUnreachable`].  Everything else is wrapped as
/// [`EmbeddingError::OllamaError`].
fn classify_error(err: ureq::Error) -> EmbeddingError {
    match err {
        ureq::Error::ConnectionFailed | ureq::Error::HostNotFound | ureq::Error::Timeout(_) => {
            EmbeddingError::OllamaUnreachable
        }
        ureq::Error::Io(ref io_err)
            if matches!(
                io_err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            EmbeddingError::OllamaUnreachable
        }
        other => EmbeddingError::OllamaError(other.to_string()),
    }
}

/// Try to extract a human-readable message from an Ollama error response body.
///
/// Ollama returns `{"error":"..."}` on failure.  If the body cannot be parsed,
/// falls back to `"HTTP {status}"`.
fn extract_error_detail(status: u16, body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(msg) = json.get("error").and_then(|v| v.as_str())
    {
        return msg.to_string();
    }
    format!("HTTP {status}")
}

// ---------------------------------------------------------------------------
// Chunking helpers
// ---------------------------------------------------------------------------

/// Extract lines from 1-based `start_line` to `end_line` (inclusive) from
/// `source`.  If `end_line` is `None`, extracts to end of file.
fn extract_line_range(source: &str, start_line: usize, end_line: Option<usize>) -> &str {
    // Find the byte offset where 1-based `line_num` starts.
    // Line 1 starts at byte 0; line N starts after the (N-1)th newline.
    let line_start_offset = |text: &str, line_num: usize| -> usize {
        if line_num <= 1 {
            return 0;
        }
        let mut count = 0usize;
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                count += 1;
                if count == line_num - 1 {
                    return i + 1;
                }
            }
        }
        // If we run out of lines, return end of text.
        text.len()
    };

    let start_idx = line_start_offset(source, start_line);

    let end_idx = match end_line {
        Some(el) => {
            // End byte is after the last byte of `el` (inclusive of that line).
            // That's the start of line el+1.
            line_start_offset(source, el + 1).min(source.len())
        }
        None => source.len(),
    };

    &source[start_idx..end_idx]
}

/// Truncate `text` to at most `max_bytes`, cutting at the last newline
/// within the budget.  If no newline is found, truncates at a char boundary.
fn truncate_at_line_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    // Try to find the last newline within the budget.
    if let Some(pos) = text[..max_bytes].rfind('\n') {
        return &text[..pos + 1];
    }
    // No newline found -- truncate at a char boundary.
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

// ---------------------------------------------------------------------------
// Public chunking API
// ---------------------------------------------------------------------------

/// Build the metadata header for a chunk.
///
/// Format: `File: <path>\nScope: <scope>\nImports: <imports>\n---\n`
/// (Scope and Imports lines are omitted when absent/empty.)
fn build_chunk_header(file: &str, scope: Option<&str>, imports_line: Option<&str>) -> String {
    let mut header = format!("File: {file}\n");
    if let Some(s) = scope {
        header.push_str(&format!("Scope: {s}\n"));
    }
    if let Some(imp) = imports_line {
        header.push_str(imp);
    }
    header.push_str("---\n");
    header
}

/// Append code to a header, truncating so the total fits within [`MAX_CHUNK_BYTES`].
fn assemble_chunk(header: String, code: &str) -> String {
    let remaining = MAX_CHUNK_BYTES.saturating_sub(header.len());
    let code = truncate_at_line_boundary(code, remaining);
    let mut chunk = header;
    chunk.push_str(code);
    chunk
}

/// Generate a context-rich text chunk for a single symbol.
///
/// Format:
/// ```text
/// File: <path>
/// Scope: <scope>       (omitted when None)
/// Imports: <imports>    (omitted when empty)
/// ---
/// <source_code>
/// ```
///
/// `source_code` is the full file content; the relevant line range is
/// extracted from `symbol.line` to `symbol.end_line`.
pub fn chunk_symbol(symbol: &Symbol, file_imports: &[String], source_code: &str) -> String {
    let imports_line = if file_imports.is_empty() {
        None
    } else {
        Some(format!("Imports: {}\n", file_imports.join(", ")))
    };
    let header = build_chunk_header(
        &symbol.file,
        symbol.scope.as_deref(),
        imports_line.as_deref(),
    );
    let code = extract_line_range(source_code, symbol.line, symbol.end_line);
    assemble_chunk(header, code)
}

/// Generate a fallback chunk for a file with no extractable symbols.
///
/// Format:
/// ```text
/// File: <path>
/// ---
/// <content>
/// ```
pub fn chunk_file_fallback(path: &str, content: &str) -> String {
    let header = build_chunk_header(path, None, None);
    assemble_chunk(header, content)
}

// ---------------------------------------------------------------------------
// DB query helpers
// ---------------------------------------------------------------------------

/// A symbol row with its database ID.
struct SymbolRow {
    id: i64,
    symbol: Symbol,
}

/// Query all symbols from the database, returning (id, Symbol) pairs.
fn query_all_symbols(conn: &Connection) -> Result<Vec<SymbolRow>, EmbeddingError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, kind, file, line, col, end_line, scope, signature, language
             FROM symbols ORDER BY file, line",
        )
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    let rows = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let name: String = row.get(1)?;
            let kind_str: String = row.get(2)?;
            let file: String = row.get(3)?;
            let line: usize = row.get::<_, i64>(4)? as usize;
            let col: usize = row.get::<_, i64>(5)? as usize;
            let end_line: Option<usize> = row.get::<_, Option<i64>>(6)?.map(|v| v as usize);
            let scope: Option<String> = row.get(7)?;
            let signature: String = row.get::<_, Option<String>>(8)?.unwrap_or_default();
            let language: String = row.get(9)?;
            let kind = kind_str
                .parse::<SymbolKind>()
                .unwrap_or(SymbolKind::Function);

            Ok(SymbolRow {
                id,
                symbol: Symbol {
                    name,
                    kind,
                    file,
                    line,
                    col,
                    end_line,
                    scope,
                    signature,
                    language,
                },
            })
        })
        .map_err(|_| EmbeddingError::ChunkingFailed)?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

/// Query file-level import paths for a given source file.
#[cfg(test)]
fn query_file_imports(conn: &Connection, source_file: &str) -> Result<Vec<String>, EmbeddingError> {
    let mut stmt = conn
        .prepare("SELECT import_path FROM file_imports WHERE source_file = ?1")
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    let imports = stmt
        .query_map([source_file], |row| row.get(0))
        .map_err(|_| EmbeddingError::ChunkingFailed)?
        .filter_map(|r| r.ok())
        .collect();

    Ok(imports)
}

/// Batch-fetch all file-level imports, grouped by source file.
fn query_all_file_imports(
    conn: &Connection,
) -> Result<BTreeMap<String, Vec<String>>, EmbeddingError> {
    let mut stmt = conn
        .prepare("SELECT source_file, import_path FROM file_imports ORDER BY source_file")
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    let mut imports: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    for r in rows.flatten() {
        imports.entry(r.0).or_default().push(r.1);
    }

    Ok(imports)
}

// ---------------------------------------------------------------------------
// Vector normalization
// ---------------------------------------------------------------------------

/// L2-normalize a vector in place.
///
/// Divides each element by the L2 (Euclidean) norm so the result has
/// unit length.  Zero-norm vectors (all zeros) are left unchanged.
pub fn normalize(vec: &mut [f32]) {
    let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        let inv_norm = 1.0 / norm;
        for x in vec.iter_mut() {
            *x *= inv_norm;
        }
    }
}

// ---------------------------------------------------------------------------
// Vector storage and retrieval
// ---------------------------------------------------------------------------

/// Store an embedding vector for a symbol.
///
/// L2-normalizes the vector before storing.  Uses `INSERT OR REPLACE`
/// so re-embedding the same symbol overwrites the previous vector.
pub fn store_embedding(
    conn: &Connection,
    symbol_id: i64,
    file: &str,
    chunk_text: &str,
    vector: &[f32],
) -> Result<(), EmbeddingError> {
    let mut normalized = vector.to_vec();
    normalize(&mut normalized);

    let bytes: &[u8] = bytemuck::cast_slice(&normalized);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    conn.execute(
        "INSERT OR REPLACE INTO embeddings (symbol_id, file, chunk_text, vector, stale, created_at)
         VALUES (?1, ?2, ?3, ?4, 0, ?5)",
        rusqlite::params![symbol_id, file, chunk_text, bytes, now],
    )
    .map_err(|_| EmbeddingError::ChunkingFailed)?;

    Ok(())
}

/// Batch-insert embedding vectors within a single transaction.
///
/// Each tuple is `(symbol_id, file, chunk_text, vector)`.  Vectors are
/// L2-normalized before storage.  The entire batch is atomic: if any
/// insert fails, all are rolled back.
pub fn store_embeddings_batch(
    conn: &Connection,
    embeddings: &[(i64, &str, &str, &[f32])],
) -> Result<(), EmbeddingError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let tx = conn
        .unchecked_transaction()
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    {
        let mut stmt = tx
            .prepare(
                "INSERT OR REPLACE INTO embeddings (symbol_id, file, chunk_text, vector, stale, created_at)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            )
            .map_err(|_| EmbeddingError::ChunkingFailed)?;

        for &(symbol_id, file, chunk_text, vector) in embeddings {
            let mut normalized = vector.to_vec();
            normalize(&mut normalized);
            let bytes: &[u8] = bytemuck::cast_slice(&normalized);
            stmt.execute(rusqlite::params![symbol_id, file, chunk_text, bytes, now])
                .map_err(|_| EmbeddingError::ChunkingFailed)?;
        }
    }

    tx.commit().map_err(|_| EmbeddingError::ChunkingFailed)?;
    Ok(())
}

/// Load all embedding vectors from the database.
///
/// Returns `(symbol_id, vector)` pairs.  Uses `bytemuck::cast_slice`
/// for zero-copy BLOB-to-f32 conversion.
pub fn load_all_embeddings(conn: &Connection) -> Result<Vec<(i64, Vec<f32>)>, EmbeddingError> {
    let mut stmt = conn
        .prepare("SELECT symbol_id, vector FROM embeddings")
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    let rows = stmt
        .query_map([], |row| {
            let symbol_id: i64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((symbol_id, blob))
        })
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    let mut results = Vec::new();
    for r in rows {
        let (symbol_id, blob) = r.map_err(|_| EmbeddingError::ChunkingFailed)?;
        let floats: &[f32] =
            bytemuck::try_cast_slice(&blob).map_err(|_| EmbeddingError::ChunkingFailed)?;
        results.push((symbol_id, floats.to_vec()));
    }

    Ok(results)
}

/// Delete all embeddings for a given file.
pub fn delete_embeddings_for_file(conn: &Connection, file: &str) -> Result<(), EmbeddingError> {
    conn.execute(
        "DELETE FROM embeddings WHERE file = ?1",
        rusqlite::params![file],
    )
    .map_err(|_| EmbeddingError::ChunkingFailed)?;
    Ok(())
}

/// Mark all embeddings for a file as stale (`stale = 1`).
pub fn mark_embeddings_stale(conn: &Connection, file: &str) -> Result<(), EmbeddingError> {
    conn.execute(
        "UPDATE embeddings SET stale = 1 WHERE file = ?1",
        rusqlite::params![file],
    )
    .map_err(|_| EmbeddingError::ChunkingFailed)?;
    Ok(())
}

/// Return `(total_count, stale_count)` for embeddings in the database.
pub fn embedding_stats(conn: &Connection) -> Result<(usize, usize), EmbeddingError> {
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    let stale: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE stale = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    Ok((total as usize, stale as usize))
}

// ---------------------------------------------------------------------------
// Public bulk chunking API
// ---------------------------------------------------------------------------

/// Compute byte offsets for each line start in `source`.
///
/// Returns a vec where `offsets[i]` is the byte offset of 1-based line `i+1`.
fn compute_line_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0]; // line 1 starts at byte 0
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            offsets.push(i + 1);
        }
    }
    offsets
}

/// Extract lines using precomputed line offsets for O(1) lookup.
fn extract_line_range_indexed<'a>(
    source: &'a str,
    offsets: &[usize],
    start_line: usize,
    end_line: Option<usize>,
) -> &'a str {
    let start_idx = if start_line <= 1 {
        0
    } else if start_line - 1 < offsets.len() {
        offsets[start_line - 1]
    } else {
        return "";
    };

    let end_idx = match end_line {
        Some(el) => {
            if el < offsets.len() {
                offsets[el].min(source.len())
            } else {
                source.len()
            }
        }
        None => source.len(),
    };

    &source[start_idx..end_idx]
}

/// Generate text chunks for all indexed symbols.
///
/// Returns `(symbol_id, chunk_text)` pairs.  Reads source files from disk
/// under `repo_root`, and silently skips files that cannot be read or whose
/// paths are absolute or contain `..` components.
pub fn chunk_all_symbols(
    conn: &Connection,
    repo_root: &Path,
) -> Result<Vec<(i64, String)>, EmbeddingError> {
    let rows = query_all_symbols(conn)?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // Batch-fetch all file imports in one query.
    let all_imports = query_all_file_imports(conn)?;

    // Group symbols by file (BTreeMap for deterministic iteration order).
    let mut by_file: BTreeMap<String, Vec<&SymbolRow>> = BTreeMap::new();
    for row in &rows {
        by_file
            .entry(row.symbol.file.clone())
            .or_default()
            .push(row);
    }

    let mut results = Vec::with_capacity(rows.len());

    for (file, symbols) in &by_file {
        // Reject absolute paths and ".." to prevent path traversal.
        let rel = Path::new(file);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }

        let path = repo_root.join(file);
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let imports: &[String] = all_imports.get(file.as_str()).map_or(&[], |v| v.as_slice());
        // Pre-compute the joined imports string once per file.
        let imports_line = if imports.is_empty() {
            None
        } else {
            Some(format!("Imports: {}\n", imports.join(", ")))
        };

        // Pre-compute line offsets once per file for O(1) extraction.
        let offsets = compute_line_offsets(&source);

        for sym_row in symbols {
            let code = extract_line_range_indexed(
                &source,
                &offsets,
                sym_row.symbol.line,
                sym_row.symbol.end_line,
            );

            let header = build_chunk_header(
                &sym_row.symbol.file,
                sym_row.symbol.scope.as_deref(),
                imports_line.as_deref(),
            );
            let chunk = assemble_chunk(header, code);
            results.push((sym_row.id, chunk));
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_client_uses_localhost() {
        let client = OllamaClient::new();
        assert_eq!(client.base_url, DEFAULT_BASE_URL);
        assert_eq!(client.model, DEFAULT_MODEL);
    }

    #[test]
    fn with_base_url_trims_trailing_slash() {
        let client = OllamaClient::with_base_url("http://example.com:11434/");
        assert_eq!(client.base_url, "http://example.com:11434");
    }

    #[test]
    fn with_base_url_preserves_clean_url() {
        let client = OllamaClient::with_base_url("http://example.com:11434");
        assert_eq!(client.base_url, "http://example.com:11434");
    }

    // -- Health check tests ---------------------------------------------------

    #[test]
    fn health_check_returns_false_when_unreachable() {
        // Port 19999 should have nothing listening.
        let client = OllamaClient::with_base_url("http://127.0.0.1:19999");
        assert!(!client.is_healthy());
    }

    // -- Connection error classification tests --------------------------------

    #[test]
    fn classify_error_connection_refused_is_unreachable() {
        let err = ureq::Error::ConnectionFailed;
        let result = classify_error(err);
        assert!(matches!(result, EmbeddingError::OllamaUnreachable));
    }

    #[test]
    fn classify_error_host_not_found_is_unreachable() {
        let err = ureq::Error::HostNotFound;
        let result = classify_error(err);
        assert!(matches!(result, EmbeddingError::OllamaUnreachable));
    }

    #[test]
    fn classify_error_timeout_is_unreachable() {
        let err = ureq::Error::Timeout(ureq::Timeout::Connect);
        let result = classify_error(err);
        assert!(matches!(result, EmbeddingError::OllamaUnreachable));
    }

    #[test]
    fn classify_error_io_connection_refused_is_unreachable() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let err = ureq::Error::Io(io_err);
        let result = classify_error(err);
        assert!(matches!(result, EmbeddingError::OllamaUnreachable));
    }

    #[test]
    fn classify_error_other_is_ollama_error() {
        let err = ureq::Error::BadUri("bad".into());
        let result = classify_error(err);
        assert!(matches!(result, EmbeddingError::OllamaError(_)));
    }

    // -- Error detail extraction tests ----------------------------------------

    #[test]
    fn extract_error_detail_parses_json_error_field() {
        let body = r#"{"error":"model not found"}"#;
        assert_eq!(extract_error_detail(400, body), "model not found");
    }

    #[test]
    fn extract_error_detail_falls_back_to_status() {
        assert_eq!(extract_error_detail(500, "not json"), "HTTP 500");
    }

    #[test]
    fn extract_error_detail_falls_back_on_missing_field() {
        let body = r#"{"status":"bad"}"#;
        assert_eq!(extract_error_detail(422, body), "HTTP 422");
    }

    // -- embed_batch tests ----------------------------------------------------

    #[test]
    fn embed_batch_empty_returns_empty_vec() {
        let client = OllamaClient::with_base_url("http://127.0.0.1:19999");
        let result = client.embed_batch(&[]);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn embed_batch_unreachable_returns_error() {
        let client = OllamaClient::with_base_url("http://127.0.0.1:19999");
        let texts = vec!["hello".to_string()];
        let result = client.embed_batch(&texts);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            EmbeddingError::OllamaUnreachable
        ));
    }

    // -- embed_single tests ---------------------------------------------------

    #[test]
    fn embed_single_unreachable_returns_error() {
        let client = OllamaClient::with_base_url("http://127.0.0.1:19999");
        let result = client.embed_single("hello");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            EmbeddingError::OllamaUnreachable
        ));
    }

    // -- Serde round-trip tests -----------------------------------------------

    #[test]
    fn embed_request_serializes_correctly() {
        let req = EmbedRequest {
            model: "test-model".to_string(),
            input: vec!["hello".to_string(), "world".to_string()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "test-model");
        assert_eq!(json["input"][0], "hello");
        assert_eq!(json["input"][1], "world");
    }

    #[test]
    fn embed_response_deserializes_correctly() {
        let json = r#"{"embeddings":[[0.1,0.2,0.3],[0.4,0.5,0.6]]}"#;
        let resp: EmbedResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.embeddings.len(), 2);
        assert_eq!(resp.embeddings[0], vec![0.1, 0.2, 0.3]);
        assert_eq!(resp.embeddings[1], vec![0.4, 0.5, 0.6]);
    }

    // -- extract_line_range tests ---------------------------------------------

    #[test]
    fn extract_line_range_single_line() {
        let src = "line1\nline2\nline3\n";
        assert_eq!(extract_line_range(src, 2, Some(2)), "line2\n");
    }

    #[test]
    fn extract_line_range_multiple_lines() {
        let src = "line1\nline2\nline3\nline4\n";
        assert_eq!(extract_line_range(src, 2, Some(3)), "line2\nline3\n");
    }

    #[test]
    fn extract_line_range_to_end_of_file() {
        let src = "line1\nline2\nline3";
        assert_eq!(extract_line_range(src, 2, None), "line2\nline3");
    }

    #[test]
    fn extract_line_range_first_line() {
        let src = "line1\nline2\n";
        assert_eq!(extract_line_range(src, 1, Some(1)), "line1\n");
    }

    #[test]
    fn extract_line_range_beyond_end() {
        let src = "line1\nline2\n";
        // end_line beyond file length should return to EOF
        assert_eq!(extract_line_range(src, 2, Some(99)), "line2\n");
    }

    // -- truncate_at_line_boundary tests --------------------------------------

    #[test]
    fn truncate_at_line_boundary_no_truncation_needed() {
        let text = "short text";
        assert_eq!(truncate_at_line_boundary(text, 100), "short text");
    }

    #[test]
    fn truncate_at_line_boundary_cuts_at_newline() {
        let text = "line1\nline2\nline3\n";
        // Budget of 12 bytes: "line1\nline2\n" is 12 bytes exactly
        assert_eq!(truncate_at_line_boundary(text, 12), "line1\nline2\n");
    }

    #[test]
    fn truncate_at_line_boundary_cuts_before_partial_line() {
        let text = "line1\nline2\nline3\n";
        // Budget of 10: can fit "line1\n" (6 bytes) but not "line1\nline2\n" (12)
        assert_eq!(truncate_at_line_boundary(text, 10), "line1\n");
    }

    #[test]
    fn truncate_at_line_boundary_no_newline_cuts_at_char() {
        let text = "abcdefghij";
        assert_eq!(truncate_at_line_boundary(text, 5), "abcde");
    }

    // -- chunk_symbol tests ---------------------------------------------------

    fn make_symbol(
        name: &str,
        file: &str,
        line: usize,
        end_line: Option<usize>,
        scope: Option<&str>,
    ) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            file: file.to_string(),
            line,
            col: 0,
            end_line,
            scope: scope.map(|s| s.to_string()),
            signature: format!("fn {name}()"),
            language: "Rust".to_string(),
        }
    }

    #[test]
    fn chunk_symbol_full_format() {
        let sym = make_symbol("foo", "src/main.rs", 3, Some(5), Some("MyStruct"));
        let source = "line1\nline2\nfn foo() {\n    42\n}\nline6\n";
        let imports = vec!["std::io".to_string(), "serde".to_string()];

        let chunk = chunk_symbol(&sym, &imports, source);
        let expected = "File: src/main.rs\nScope: MyStruct\nImports: std::io, serde\n---\nfn foo() {\n    42\n}\n";
        assert_eq!(chunk, expected);
    }

    #[test]
    fn chunk_symbol_no_scope() {
        let sym = make_symbol("bar", "lib.rs", 1, Some(2), None);
        let source = "fn bar() {\n    0\n}\n";
        let imports = vec!["os".to_string()];

        let chunk = chunk_symbol(&sym, &imports, source);
        // No Scope line when scope is None
        assert!(!chunk.contains("Scope:"));
        assert!(chunk.starts_with("File: lib.rs\nImports: os\n---\n"));
    }

    #[test]
    fn chunk_symbol_no_imports() {
        let sym = make_symbol("baz", "app.py", 1, Some(1), Some("App"));
        let source = "def baz(): pass\n";
        let imports: Vec<String> = vec![];

        let chunk = chunk_symbol(&sym, &imports, source);
        // No Imports line when imports is empty
        assert!(!chunk.contains("Imports:"));
        assert!(chunk.starts_with("File: app.py\nScope: App\n---\n"));
    }

    #[test]
    fn chunk_symbol_no_scope_no_imports() {
        let sym = make_symbol("x", "a.rs", 1, Some(1), None);
        let source = "let x = 1;\n";

        let chunk = chunk_symbol(&sym, &[], source);
        assert_eq!(chunk, "File: a.rs\n---\nlet x = 1;\n");
    }

    #[test]
    fn chunk_symbol_truncates_long_source() {
        let sym = make_symbol("big", "big.rs", 1, None, None);
        // Create source larger than MAX_CHUNK_BYTES
        let long_line = "x".repeat(1000);
        let lines: Vec<String> = (0..40).map(|i| format!("{long_line}_{i}")).collect();
        let source = lines.join("\n");

        let chunk = chunk_symbol(&sym, &[], &source);
        assert!(chunk.len() <= MAX_CHUNK_BYTES);
        // Should still have the header
        assert!(chunk.starts_with("File: big.rs\n---\n"));
    }

    // -- chunk_file_fallback tests --------------------------------------------

    #[test]
    fn chunk_file_fallback_basic() {
        let chunk = chunk_file_fallback("readme.txt", "Hello world\n");
        assert_eq!(chunk, "File: readme.txt\n---\nHello world\n");
    }

    #[test]
    fn chunk_file_fallback_truncates_long_content() {
        let content = "x".repeat(MAX_CHUNK_BYTES + 1000);
        let chunk = chunk_file_fallback("huge.txt", &content);
        assert!(chunk.len() <= MAX_CHUNK_BYTES);
        assert!(chunk.starts_with("File: huge.txt\n---\n"));
    }

    // -- DB helper tests ------------------------------------------------------

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE symbols (
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
            CREATE TABLE file_imports (
                id INTEGER PRIMARY KEY,
                source_file TEXT NOT NULL,
                import_path TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn insert_symbol(
        conn: &Connection,
        name: &str,
        kind: &str,
        file: &str,
        line: i64,
        end_line: Option<i64>,
        scope: Option<&str>,
        signature: &str,
        language: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, end_line, scope, signature, language)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, ?8)",
            rusqlite::params![name, kind, file, line, end_line, scope, signature, language],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_import(conn: &Connection, source_file: &str, import_path: &str) {
        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params![source_file, import_path],
        )
        .unwrap();
    }

    #[test]
    fn query_all_symbols_returns_all_rows() {
        let conn = setup_test_db();
        insert_symbol(
            &conn,
            "foo",
            "function",
            "a.rs",
            1,
            Some(3),
            None,
            "fn foo()",
            "Rust",
        );
        insert_symbol(
            &conn,
            "bar",
            "method",
            "b.rs",
            5,
            Some(10),
            Some("Baz"),
            "fn bar()",
            "Rust",
        );

        let rows = query_all_symbols(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].symbol.name, "foo");
        assert_eq!(rows[1].symbol.name, "bar");
        assert_eq!(rows[1].symbol.scope, Some("Baz".to_string()));
    }

    #[test]
    fn query_file_imports_returns_imports_for_file() {
        let conn = setup_test_db();
        insert_import(&conn, "a.rs", "std::io");
        insert_import(&conn, "a.rs", "serde");
        insert_import(&conn, "b.rs", "tokio");

        let imports = query_file_imports(&conn, "a.rs").unwrap();
        assert_eq!(imports.len(), 2);
        assert!(imports.contains(&"std::io".to_string()));
        assert!(imports.contains(&"serde".to_string()));
    }

    #[test]
    fn query_file_imports_empty_for_unknown_file() {
        let conn = setup_test_db();
        let imports = query_file_imports(&conn, "unknown.rs").unwrap();
        assert!(imports.is_empty());
    }

    // -- chunk_all_symbols tests ----------------------------------------------

    #[test]
    fn chunk_all_symbols_basic() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        // Write a source file.
        std::fs::write(
            root.join("main.rs"),
            "fn hello() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();

        let conn = setup_test_db();
        let sym_id = insert_symbol(
            &conn,
            "hello",
            "function",
            "main.rs",
            1,
            Some(3),
            None,
            "fn hello()",
            "Rust",
        );
        insert_import(&conn, "main.rs", "std::io");

        let chunks = chunk_all_symbols(&conn, root).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].0, sym_id);
        assert!(chunks[0].1.contains("File: main.rs"));
        assert!(chunks[0].1.contains("Imports: std::io"));
        assert!(chunks[0].1.contains("fn hello()"));
    }

    #[test]
    fn chunk_all_symbols_skips_unreadable_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        // Don't write the source file -- it should be silently skipped.

        let conn = setup_test_db();
        insert_symbol(
            &conn,
            "ghost",
            "function",
            "missing.rs",
            1,
            Some(1),
            None,
            "fn ghost()",
            "Rust",
        );

        let chunks = chunk_all_symbols(&conn, root).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_all_symbols_multiple_symbols_same_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::fs::write(root.join("lib.rs"), "fn a() { 1 }\nfn b() { 2 }\n").unwrap();

        let conn = setup_test_db();
        let id_a = insert_symbol(
            &conn,
            "a",
            "function",
            "lib.rs",
            1,
            Some(1),
            None,
            "fn a()",
            "Rust",
        );
        let id_b = insert_symbol(
            &conn,
            "b",
            "function",
            "lib.rs",
            2,
            Some(2),
            None,
            "fn b()",
            "Rust",
        );

        let chunks = chunk_all_symbols(&conn, root).unwrap();
        assert_eq!(chunks.len(), 2);
        let ids: Vec<i64> = chunks.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&id_a));
        assert!(ids.contains(&id_b));
    }

    #[test]
    fn chunk_all_symbols_rejects_path_traversal() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        let conn = setup_test_db();
        insert_symbol(
            &conn,
            "evil",
            "function",
            "../../../etc/passwd",
            1,
            Some(1),
            None,
            "fn evil()",
            "Rust",
        );

        let chunks = chunk_all_symbols(&conn, root).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_all_symbols_rejects_absolute_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        let conn = setup_test_db();
        insert_symbol(
            &conn,
            "evil",
            "function",
            "/etc/passwd",
            1,
            Some(1),
            None,
            "fn evil()",
            "Rust",
        );

        let chunks = chunk_all_symbols(&conn, root).unwrap();
        assert!(chunks.is_empty());
    }

    // -- line offset tests ----------------------------------------------------

    #[test]
    fn compute_line_offsets_basic() {
        let source = "line1\nline2\nline3\n";
        let offsets = compute_line_offsets(source);
        assert_eq!(offsets, vec![0, 6, 12, 18]);
    }

    #[test]
    fn extract_line_range_indexed_single_line() {
        let source = "line1\nline2\nline3\n";
        let offsets = compute_line_offsets(source);
        assert_eq!(
            extract_line_range_indexed(source, &offsets, 2, Some(2)),
            "line2\n"
        );
    }

    #[test]
    fn extract_line_range_indexed_to_end() {
        let source = "line1\nline2\nline3";
        let offsets = compute_line_offsets(source);
        assert_eq!(
            extract_line_range_indexed(source, &offsets, 2, None),
            "line2\nline3"
        );
    }

    // -- normalize tests ------------------------------------------------------

    #[test]
    fn normalize_produces_unit_norm() {
        let mut v = vec![3.0_f32, 4.0];
        normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn normalize_zero_vector_stays_zero() {
        let mut v = vec![0.0_f32, 0.0, 0.0];
        normalize(&mut v);
        assert!(v.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn normalize_already_unit_vector() {
        let mut v = vec![1.0_f32, 0.0, 0.0];
        normalize(&mut v);
        assert!((v[0] - 1.0).abs() < 1e-6);
        assert!(v[1].abs() < 1e-6);
        assert!(v[2].abs() < 1e-6);
    }

    #[test]
    fn normalize_empty_vector() {
        let mut v: Vec<f32> = vec![];
        normalize(&mut v); // Should not panic
        assert!(v.is_empty());
    }

    // -- DB embedding storage tests -------------------------------------------

    fn setup_test_db_with_embeddings() -> Connection {
        let conn = setup_test_db();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
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
            CREATE INDEX IF NOT EXISTS idx_embeddings_file ON embeddings(file);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn store_embedding_round_trip() {
        let conn = setup_test_db_with_embeddings();
        let sym_id = insert_symbol(
            &conn,
            "foo",
            "function",
            "a.rs",
            1,
            Some(3),
            None,
            "fn foo()",
            "Rust",
        );

        let vector = vec![3.0_f32, 4.0]; // will be normalized to [0.6, 0.8]
        store_embedding(&conn, sym_id, "a.rs", "fn foo() {}", &vector).unwrap();

        let loaded = load_all_embeddings(&conn).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, sym_id);
        // Check it was L2-normalized
        let norm: f32 = loaded[0].1.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
        assert!((loaded[0].1[0] - 0.6).abs() < 1e-6);
        assert!((loaded[0].1[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn store_embedding_replaces_on_same_symbol() {
        let conn = setup_test_db_with_embeddings();
        let sym_id = insert_symbol(
            &conn,
            "foo",
            "function",
            "a.rs",
            1,
            Some(1),
            None,
            "fn foo()",
            "Rust",
        );

        let v1 = vec![1.0_f32, 0.0];
        store_embedding(&conn, sym_id, "a.rs", "v1", &v1).unwrap();

        let v2 = vec![0.0_f32, 1.0];
        store_embedding(&conn, sym_id, "a.rs", "v2", &v2).unwrap();

        let loaded = load_all_embeddings(&conn).unwrap();
        assert_eq!(loaded.len(), 1);
        // Should have the second vector
        assert!((loaded[0].1[0] - 0.0).abs() < 1e-6);
        assert!((loaded[0].1[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn store_embeddings_batch_inserts_all() {
        let conn = setup_test_db_with_embeddings();
        let id1 = insert_symbol(
            &conn,
            "a",
            "function",
            "a.rs",
            1,
            Some(1),
            None,
            "fn a()",
            "Rust",
        );
        let id2 = insert_symbol(
            &conn,
            "b",
            "function",
            "b.rs",
            1,
            Some(1),
            None,
            "fn b()",
            "Rust",
        );

        let v1 = vec![1.0_f32, 0.0];
        let v2 = vec![0.0_f32, 1.0];
        let batch: Vec<(i64, &str, &str, &[f32])> =
            vec![(id1, "a.rs", "fn a()", &v1), (id2, "b.rs", "fn b()", &v2)];
        store_embeddings_batch(&conn, &batch).unwrap();

        let loaded = load_all_embeddings(&conn).unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn store_embeddings_batch_is_atomic() {
        let conn = setup_test_db_with_embeddings();
        let id1 = insert_symbol(
            &conn,
            "a",
            "function",
            "a.rs",
            1,
            Some(1),
            None,
            "fn a()",
            "Rust",
        );

        let v1 = vec![1.0_f32, 0.0];
        // Second entry references non-existent symbol_id=999 -- should cause FK failure.
        let v2 = vec![0.0_f32, 1.0];
        let batch: Vec<(i64, &str, &str, &[f32])> =
            vec![(id1, "a.rs", "fn a()", &v1), (999, "z.rs", "bogus", &v2)];
        let result = store_embeddings_batch(&conn, &batch);
        assert!(result.is_err());

        // Atomic: nothing should have been inserted.
        let loaded = load_all_embeddings(&conn).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_all_embeddings_empty_db() {
        let conn = setup_test_db_with_embeddings();
        let loaded = load_all_embeddings(&conn).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn delete_embeddings_for_file_removes_correct_rows() {
        let conn = setup_test_db_with_embeddings();
        let id1 = insert_symbol(
            &conn,
            "a",
            "function",
            "a.rs",
            1,
            Some(1),
            None,
            "fn a()",
            "Rust",
        );
        let id2 = insert_symbol(
            &conn,
            "b",
            "function",
            "b.rs",
            1,
            Some(1),
            None,
            "fn b()",
            "Rust",
        );

        store_embedding(&conn, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        store_embedding(&conn, id2, "b.rs", "fn b()", &[0.0, 1.0]).unwrap();

        delete_embeddings_for_file(&conn, "a.rs").unwrap();

        let loaded = load_all_embeddings(&conn).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, id2);
    }

    #[test]
    fn delete_embeddings_for_nonexistent_file_succeeds() {
        let conn = setup_test_db_with_embeddings();
        // Should not error even if no rows match.
        delete_embeddings_for_file(&conn, "nonexistent.rs").unwrap();
    }

    #[test]
    fn mark_embeddings_stale_sets_flag() {
        let conn = setup_test_db_with_embeddings();
        let id1 = insert_symbol(
            &conn,
            "a",
            "function",
            "a.rs",
            1,
            Some(1),
            None,
            "fn a()",
            "Rust",
        );
        let id2 = insert_symbol(
            &conn,
            "b",
            "function",
            "b.rs",
            1,
            Some(1),
            None,
            "fn b()",
            "Rust",
        );

        store_embedding(&conn, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        store_embedding(&conn, id2, "b.rs", "fn b()", &[0.0, 1.0]).unwrap();

        mark_embeddings_stale(&conn, "a.rs").unwrap();

        let (total, stale) = embedding_stats(&conn).unwrap();
        assert_eq!(total, 2);
        assert_eq!(stale, 1);
    }

    #[test]
    fn embedding_stats_all_zero() {
        let conn = setup_test_db_with_embeddings();
        let (total, stale) = embedding_stats(&conn).unwrap();
        assert_eq!(total, 0);
        assert_eq!(stale, 0);
    }

    #[test]
    fn embedding_stats_counts_correctly() {
        let conn = setup_test_db_with_embeddings();
        let id1 = insert_symbol(
            &conn,
            "a",
            "function",
            "a.rs",
            1,
            Some(1),
            None,
            "fn a()",
            "Rust",
        );
        let id2 = insert_symbol(
            &conn,
            "b",
            "function",
            "a.rs",
            2,
            Some(2),
            None,
            "fn b()",
            "Rust",
        );
        let id3 = insert_symbol(
            &conn,
            "c",
            "function",
            "b.rs",
            1,
            Some(1),
            None,
            "fn c()",
            "Rust",
        );

        store_embedding(&conn, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        store_embedding(&conn, id2, "a.rs", "fn b()", &[0.0, 1.0]).unwrap();
        store_embedding(&conn, id3, "b.rs", "fn c()", &[0.7, 0.7]).unwrap();

        mark_embeddings_stale(&conn, "a.rs").unwrap();

        let (total, stale) = embedding_stats(&conn).unwrap();
        assert_eq!(total, 3);
        assert_eq!(stale, 2);
    }

    #[test]
    fn bytemuck_round_trip_preserves_values() {
        // Verify that cast_slice/try_cast_slice round-trips correctly.
        let original: Vec<f32> = vec![1.0, 2.5, -3.0, 0.0];
        let bytes: &[u8] = bytemuck::cast_slice(&original);
        let recovered: &[f32] = bytemuck::cast_slice(bytes);
        assert_eq!(original.as_slice(), recovered);
    }
}
