//! Embedding providers, symbol chunking, and vector storage.
//!
//! Provides a bundled in-process default and a synchronous opt-in Ollama
//! client. Both implement the same provider contract so vector-space metadata
//! and the indexing pipeline remain provider-neutral.
//!
//! Also provides the chunking pipeline that transforms indexed symbols into
//! context-rich text chunks suitable for embedding by `nomic-embed-text`.

use std::collections::{BTreeMap, HashSet};
use std::io::Read as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use ureq::Agent;

pub use crate::bundled_embedding::BundledProvider;
use crate::errors::EmbeddingError;
use crate::types::{Symbol, SymbolKind};

/// Maximum chunk size in bytes.  `nomic-embed-text` supports 8192 tokens;
/// at ~4 bytes/token this gives 32 KB.
pub const MAX_CHUNK_BYTES: usize = 32_768;

/// Default Ollama server URL.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Default embedding model.
pub const DEFAULT_MODEL: &str = "nomic-embed-text";

/// Dimension produced by the legacy `nomic-embed-text` Ollama provider.
pub const OLLAMA_DIM: usize = 768;

/// User-facing error message when the configured Ollama provider is needed
/// but unreachable. Ollama is a quality tier, not a requirement: the bundled
/// provider is always a working alternative via re-embed.
pub const OLLAMA_UNREACHABLE_MSG: &str = "configured embedding provider 'ollama' is \
    unreachable; start Ollama ('ollama serve'), or re-embed with the bundled provider: \
    `wonk update --force --provider bundled`";

/// Stderr warning emitted when a query degrades to the bundled provider
/// because the configured Ollama provider is unreachable (PRD-EMB-REQ-009).
pub const BUNDLED_FALLBACK_WARNING: &str = "configured embedding provider 'ollama' is \
    unreachable; falling back to the bundled provider for this query — start Ollama \
    ('ollama serve') to restore the higher-quality tier";

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

/// Embedding implementation selected for an invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingProviderKind {
    #[default]
    Bundled,
    Ollama,
}

impl EmbeddingProviderKind {
    /// Parse a provider name as written in tool arguments and configs.
    ///
    /// Single source of truth for the string form of a provider selection,
    /// beside the enum it names: transport surfaces (MCP) parse their
    /// caller-supplied names through this instead of a transport-local
    /// match, so adding or renaming a provider touches exactly one module.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "bundled" => Ok(EmbeddingProviderKind::Bundled),
            "ollama" => Ok(EmbeddingProviderKind::Ollama),
            other => Err(format!("invalid embedding provider: {other}")),
        }
    }
}

/// Provider-neutral embedding generation contract.
pub trait EmbeddingProvider: Send + Sync {
    fn name(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed_batch(&self, chunks: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError>;

    fn embed_single(&self, chunk: &str) -> Result<Vec<f32>, EmbeddingError> {
        let mut results = self.embed_batch(&[chunk.to_string()])?;
        results.pop().ok_or(EmbeddingError::InvalidResponse)
    }

    /// Whether the provider is currently available.
    ///
    /// In-process providers use the default. Network providers override this
    /// so background and index-building workflows can retain graceful skips.
    fn is_healthy(&self) -> bool {
        true
    }

    /// Quick health probe with a short timeout for interactive paths.
    ///
    /// Defaults to [`Self::is_healthy`]; network providers override with a
    /// tighter bound so query-time planning never stalls on a dead server.
    fn is_healthy_quick(&self) -> bool {
        self.is_healthy()
    }
}

/// Resolve invocation precedence: explicit override, then configured value.
pub fn resolve_provider_kind(
    invocation: Option<EmbeddingProviderKind>,
    configured: EmbeddingProviderKind,
) -> EmbeddingProviderKind {
    invocation.unwrap_or(configured)
}

/// Construct the selected provider.
pub fn create_provider(
    kind: EmbeddingProviderKind,
) -> Result<Box<dyn EmbeddingProvider>, EmbeddingError> {
    match kind {
        EmbeddingProviderKind::Ollama => Ok(Box::new(OllamaProvider::new())),
        EmbeddingProviderKind::Bundled => Ok(Box::new(BundledProvider)),
    }
}

// ---------------------------------------------------------------------------
// Query-time provider resolution
// ---------------------------------------------------------------------------

/// One distinct vector space present in the embeddings table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVectorSpace {
    pub provider: String,
    pub dim: usize,
    pub rows: usize,
}

/// What to do about the embedding provider for a semantic query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryProviderDecision {
    /// Use the configured provider as-is.
    Active,
    /// Substitute the bundled provider for this query (PRD-EMB-REQ-009).
    BundledFallback,
    /// Refuse: stored vectors belong to a different space (PRD-EMB-REQ-005).
    Block {
        active_provider: String,
        active_dim: usize,
        stored_provider: String,
        stored_dim: usize,
    },
}

/// Decide which provider a query should run against.
///
/// Pure decision table over the configured provider, its health, and the
/// vector spaces present in the index — no I/O, so every row is unit-testable.
///
/// The fallback rule is strict: an unreachable configured provider may degrade
/// to the bundled provider only when doing so never crosses a vector space.
/// Falling back while foreign-space rows exist would either silently drop the
/// user's indexed corpus from results or store the query in the wrong space,
/// so that path blocks with a re-embed instruction instead.
pub fn decide_query_provider(
    configured: EmbeddingProviderKind,
    active_healthy: bool,
    stored: &[StoredVectorSpace],
) -> QueryProviderDecision {
    let active = match configured {
        EmbeddingProviderKind::Bundled => ("bundled", BundledProvider.dim()),
        EmbeddingProviderKind::Ollama => ("ollama", OLLAMA_DIM),
    };

    match configured {
        // A healthy active provider uses its own space when present; a
        // non-empty table without it is a provider switch, which blocks.
        EmbeddingProviderKind::Bundled => use_active_or_block(active, stored),
        EmbeddingProviderKind::Ollama if active_healthy => use_active_or_block(active, stored),
        // Unreachable Ollama degrades to bundled — but only when no
        // foreign-space rows exist that the fallback would strand.
        EmbeddingProviderKind::Ollama => {
            if stored.is_empty() || stored.iter().all(is_bundled) {
                QueryProviderDecision::BundledFallback
            } else {
                let foreign: Vec<_> = stored.iter().filter(|s| !is_bundled(s)).cloned().collect();
                block(("bundled", BundledProvider.dim()), dominant_of(&foreign))
            }
        }
    }
}

fn use_active_or_block(
    active: (&str, usize),
    stored: &[StoredVectorSpace],
) -> QueryProviderDecision {
    let has_active_space = stored
        .iter()
        .any(|s| s.provider == active.0 && s.dim == active.1);
    if stored.is_empty() || has_active_space {
        QueryProviderDecision::Active
    } else {
        block(active, dominant_of(stored))
    }
}

fn is_bundled(space: &StoredVectorSpace) -> bool {
    space.provider == "bundled" && space.dim == BundledProvider.dim()
}

fn block(active: (&str, usize), stored: StoredVectorSpace) -> QueryProviderDecision {
    QueryProviderDecision::Block {
        active_provider: active.0.to_string(),
        active_dim: active.1,
        stored_provider: stored.provider,
        stored_dim: stored.dim,
    }
}

/// The stored space with the most rows; callers guarantee `stored` is non-empty.
fn dominant_of(stored: &[StoredVectorSpace]) -> StoredVectorSpace {
    stored
        .iter()
        .max_by_key(|s| s.rows)
        .cloned()
        .expect("dominant_of requires a non-empty slice")
}

/// List the distinct vector spaces present in the embeddings table,
/// ordered by row count descending (dominant space first).
pub fn stored_vector_spaces(conn: &Connection) -> Result<Vec<StoredVectorSpace>, EmbeddingError> {
    let mut stmt = conn
        .prepare(
            "SELECT provider, dim, COUNT(*) AS rows
             FROM embeddings
             GROUP BY provider, dim
             ORDER BY rows DESC",
        )
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

    let spaces = stmt
        .query_map([], |row| {
            Ok(StoredVectorSpace {
                provider: row.get(0)?,
                dim: row.get::<_, i64>(1)? as usize,
                rows: row.get::<_, i64>(2)? as usize,
            })
        })
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(spaces)
}

/// The resolved provider for one semantic query.
pub struct QueryProviderPlan {
    pub provider: Box<dyn EmbeddingProvider>,
    /// Set when PRD-EMB-REQ-009 fallback was applied.
    pub fallback_warning: Option<&'static str>,
}

/// Resolve the provider for a semantic query against this index.
///
/// Health-checks the configured provider (Ollama via a 500 ms probe; the
/// bundled provider never touches the network), runs the pure decision
/// table, and maps a refused space through
/// [`EmbeddingError::VectorSpaceMismatch`], whose message carries the exact
/// re-embed command.
pub fn plan_query_provider(
    conn: &Connection,
    configured: EmbeddingProviderKind,
) -> Result<QueryProviderPlan, EmbeddingError> {
    let stored = stored_vector_spaces(conn)?;
    let probe = create_provider(configured)?;
    let decision = decide_query_provider(configured, probe.is_healthy_quick(), &stored);
    plan_from_decision(configured, decision)
}

/// Re-plan after the configured provider died mid-query.
///
/// The health check is skipped (we just watched the request fail), so an
/// Ollama configuration degrades to the bundled provider — or blocks when
/// the stored vectors make that unsafe.
pub fn fallback_after_disconnect(
    conn: &Connection,
    configured: EmbeddingProviderKind,
) -> Result<QueryProviderPlan, EmbeddingError> {
    let stored = stored_vector_spaces(conn)?;
    let decision = decide_query_provider(configured, false, &stored);
    plan_from_decision(configured, decision)
}

fn plan_from_decision(
    configured: EmbeddingProviderKind,
    decision: QueryProviderDecision,
) -> Result<QueryProviderPlan, EmbeddingError> {
    match decision {
        QueryProviderDecision::Active => Ok(QueryProviderPlan {
            provider: create_provider(configured)?,
            fallback_warning: None,
        }),
        QueryProviderDecision::BundledFallback => Ok(QueryProviderPlan {
            provider: create_provider(EmbeddingProviderKind::Bundled)?,
            fallback_warning: Some(BUNDLED_FALLBACK_WARNING),
        }),
        QueryProviderDecision::Block {
            active_provider,
            active_dim,
            stored_provider,
            stored_dim,
        } => Err(EmbeddingError::VectorSpaceMismatch {
            active_provider,
            active_dim,
            stored_provider,
            stored_dim,
        }),
    }
}

/// Synchronous HTTP client for Ollama's embedding API.
pub struct OllamaProvider {
    agent: Agent,
    pub(crate) base_url: String,
    pub(crate) model: String,
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaProvider {
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

    /// Quick health check with a shorter timeout (500ms) for status queries.
    ///
    /// Avoids blocking `wonk status` for the full 2-second connect timeout
    /// when Ollama is unreachable. The global bound caps the whole probe —
    /// DNS, connect, and the header read — so a wedged server that accepts
    /// the connection but never responds is reported unhealthy instead of
    /// hanging the query-planning path indefinitely.
    pub fn is_healthy_quick(&self) -> bool {
        let quick: Agent = Agent::config_builder()
            .timeout_connect(Some(Duration::from_millis(500)))
            .timeout_global(Some(Duration::from_millis(500)))
            .http_status_as_error(false)
            .build()
            .into();
        let url = format!("{}/", self.base_url);
        match quick.get(&url).call() {
            Ok(resp) => resp.status() == 200,
            Err(_) => false,
        }
    }

    fn embed_batch_impl(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        const MAX_EMBED_BYTES: usize = 32_768; // ~8192 tokens at ~4 bytes/token
        for text in texts {
            if text.len() > MAX_EMBED_BYTES {
                return Err(EmbeddingError::OllamaError(format!(
                    "input text too long ({} bytes, max {})",
                    text.len(),
                    MAX_EMBED_BYTES
                )));
            }
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
            let body = {
                let mut buf = String::new();
                let _ = response
                    .into_body()
                    .as_reader()
                    .take(4096)
                    .read_to_string(&mut buf);
                buf
            };
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

    /// Generate embeddings for a batch of texts.
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        self.embed_batch_impl(texts)
    }

    /// Generate an embedding for one text.
    pub fn embed_single(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        EmbeddingProvider::embed_single(self, text)
    }
}

impl EmbeddingProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    fn dim(&self) -> usize {
        OLLAMA_DIM
    }

    fn embed_batch(&self, chunks: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        self.embed_batch_impl(chunks)
    }

    fn is_healthy(&self) -> bool {
        OllamaProvider::is_healthy(self)
    }

    fn is_healthy_quick(&self) -> bool {
        OllamaProvider::is_healthy_quick(self)
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
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::PermissionDenied
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

/// Check whether an [`EmbeddingError`] indicates that the input exceeded
/// the model's context length.
///
/// Matches both Ollama's server-side message ("context length") and wonk's
/// own pre-flight check ("input text too long").
pub(crate) fn is_context_length_error(err: &EmbeddingError) -> bool {
    match err {
        EmbeddingError::OllamaError(msg) => {
            let m = msg.to_lowercase();
            m.contains("context length") || m.contains("input text too long")
        }
        _ => false,
    }
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

/// Map a single database row to a [`SymbolRow`].
///
/// Expects columns in this order:
/// `id, name, kind, file, line, col, end_line, scope, signature, language`.
fn map_symbol_row(row: &rusqlite::Row) -> rusqlite::Result<SymbolRow> {
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
            doc_comment: None,
        },
    })
}

/// Execute a symbol query and map rows into `SymbolRow` structs.
///
/// The `sql` must select columns in this order:
/// `id, name, kind, file, line, col, end_line, scope, signature, language`.
fn query_symbol_rows(conn: &Connection, sql: &str) -> Result<Vec<SymbolRow>, EmbeddingError> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    let rows = stmt
        .query_map([], map_symbol_row)
        .map_err(|_| EmbeddingError::ChunkingFailed)?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

/// Query symbols that do not have fresh (non-stale) embeddings.
///
/// Returns symbols whose `id` is not in the `embeddings` table with `stale = 0`.
/// This includes symbols with no embedding and symbols whose embedding is stale.
fn query_unembedded_symbols(
    conn: &Connection,
    provider: &dyn EmbeddingProvider,
) -> Result<Vec<SymbolRow>, EmbeddingError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, kind, file, line, col, end_line, scope, signature, language
             FROM symbols
             WHERE id NOT IN (
                 SELECT symbol_id FROM embeddings
                 WHERE NOT stale AND provider = ?1 AND dim = ?2
             )
             ORDER BY file, line",
        )
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    let rows = stmt
        .query_map(
            rusqlite::params![provider.name(), provider.dim() as i64],
            map_symbol_row,
        )
        .map_err(|_| EmbeddingError::ChunkingFailed)?
        .filter_map(|row| row.ok())
        .collect();
    Ok(rows)
}

/// Query symbols for specific files, returning (id, Symbol) pairs.
fn query_symbols_for_files(
    conn: &Connection,
    files: &[String],
) -> Result<Vec<SymbolRow>, EmbeddingError> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders: Vec<String> = (1..=files.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "SELECT id, name, kind, file, line, col, end_line, scope, signature, language
         FROM symbols WHERE file IN ({}) ORDER BY file, line",
        placeholders.join(", ")
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    let params: Vec<&dyn rusqlite::types::ToSql> = files
        .iter()
        .map(|f| f as &dyn rusqlite::types::ToSql)
        .collect();

    let rows = stmt
        .query_map(params.as_slice(), map_symbol_row)
        .map_err(|_| EmbeddingError::ChunkingFailed)?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

/// Query all symbols from the database, returning (id, Symbol) pairs.
fn query_all_symbols(conn: &Connection) -> Result<Vec<SymbolRow>, EmbeddingError> {
    query_symbol_rows(
        conn,
        "SELECT id, name, kind, file, line, col, end_line, scope, signature, language
         FROM symbols ORDER BY file, line",
    )
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

/// Batch-fetch file-level imports for specific files, grouped by source file.
fn query_file_imports_for_files(
    conn: &Connection,
    files: &[String],
) -> Result<BTreeMap<String, Vec<String>>, EmbeddingError> {
    if files.is_empty() {
        return Ok(BTreeMap::new());
    }
    let placeholders: Vec<String> = (1..=files.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "SELECT source_file, import_path FROM file_imports WHERE source_file IN ({}) ORDER BY source_file",
        placeholders.join(", ")
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    let params: Vec<&dyn rusqlite::types::ToSql> = files
        .iter()
        .map(|f| f as &dyn rusqlite::types::ToSql)
        .collect();

    let mut imports: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let rows = stmt
        .query_map(params.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| EmbeddingError::ChunkingFailed)?;

    for r in rows.flatten() {
        imports.entry(r.0).or_default().push(r.1);
    }

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
    provider: &dyn EmbeddingProvider,
    symbol_id: i64,
    file: &str,
    chunk_text: &str,
    vector: &[f32],
) -> Result<(), EmbeddingError> {
    validate_vector_dimension(provider, vector)?;
    let mut normalized = vector.to_vec();
    normalize(&mut normalized);

    let bytes: &[u8] = bytemuck::cast_slice(&normalized);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    conn.execute(
        "INSERT OR REPLACE INTO embeddings
            (symbol_id, file, chunk_text, vector, stale, created_at, provider, dim)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7)",
        rusqlite::params![
            symbol_id,
            file,
            chunk_text,
            bytes,
            now,
            provider.name(),
            provider.dim() as i64
        ],
    )
    .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

    Ok(())
}

/// Batch-insert embedding vectors within a single transaction.
///
/// Each tuple is `(symbol_id, file, chunk_text, vector)`.  Vectors are
/// L2-normalized before storage.  The entire batch is atomic: if any
/// insert fails, all are rolled back.
pub fn store_embeddings_batch(
    conn: &Connection,
    provider: &dyn EmbeddingProvider,
    embeddings: &[(i64, &str, &str, &[f32])],
) -> Result<(), EmbeddingError> {
    for &(_, _, _, vector) in embeddings {
        validate_vector_dimension(provider, vector)?;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

    {
        let mut stmt = tx
            .prepare(
                "INSERT OR REPLACE INTO embeddings
                    (symbol_id, file, chunk_text, vector, stale, created_at, provider, dim)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7)",
            )
            .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

        let mut scratch = Vec::new();
        for &(symbol_id, file, chunk_text, vector) in embeddings {
            scratch.clear();
            scratch.extend_from_slice(vector);
            normalize(&mut scratch);
            let bytes: &[u8] = bytemuck::cast_slice(&scratch);
            stmt.execute(rusqlite::params![
                symbol_id,
                file,
                chunk_text,
                bytes,
                now,
                provider.name(),
                provider.dim() as i64
            ])
            .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;
        }
    }

    tx.commit()
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;
    Ok(())
}

fn validate_vector_dimension(
    provider: &dyn EmbeddingProvider,
    vector: &[f32],
) -> Result<(), EmbeddingError> {
    if vector.len() != provider.dim() {
        return Err(EmbeddingError::VectorDimension {
            provider: provider.name().to_string(),
            expected: provider.dim(),
            actual: vector.len(),
        });
    }
    Ok(())
}

fn decode_vector(
    blob: &[u8],
    provider: &dyn EmbeddingProvider,
) -> Result<Vec<f32>, EmbeddingError> {
    let floats: &[f32] = bytemuck::try_cast_slice(blob)
        .map_err(|e| EmbeddingError::StorageFailed(format!("BLOB cast failed: {e}")))?;
    if floats.len() != provider.dim() {
        return Err(vector_space_mismatch(
            provider,
            provider.name().to_string(),
            floats.len() as i64,
        ));
    }
    Ok(floats.to_vec())
}

fn vector_space_mismatch(
    provider: &dyn EmbeddingProvider,
    stored_provider: String,
    stored_dim: i64,
) -> EmbeddingError {
    EmbeddingError::VectorSpaceMismatch {
        active_provider: provider.name().to_string(),
        active_dim: provider.dim(),
        stored_provider,
        stored_dim: stored_dim.max(0) as usize,
    }
}

/// Shared provider-scoped vector load behind the all / path-prefix /
/// file-set loaders.
///
/// `scope_condition` is a SQL predicate (no `WHERE`/`AND` keywords) that
/// narrows the requested row set — e.g. `file GLOB ?1 AND NOT stale`; its
/// bind parameters come first in `scope_params`. The helper appends the
/// provider-space predicates itself, numbering the provider and dim
/// parameters after the scope's.
///
/// Vector-space invariant (DR-032 / PRD-EMB-REQ-005): before any compatible
/// row is returned, the same scope is probed for a row from a different
/// `(provider, dim)` space. Finding one fails fast with
/// [`EmbeddingError::VectorSpaceMismatch`] — whether it sits alongside
/// compatible rows (a partially migrated index) or alone (a provider
/// switch) — so a query never silently serves a subset of the corpus.
fn load_scoped_embeddings(
    conn: &Connection,
    provider: &dyn EmbeddingProvider,
    scope_condition: Option<&str>,
    scope_params: &[&dyn rusqlite::types::ToSql],
) -> Result<Vec<(i64, Vec<f32>)>, EmbeddingError> {
    let provider_param = scope_params.len() + 1;
    let dim_param = scope_params.len() + 2;
    let scope = match scope_condition {
        Some(condition) => format!("{condition} AND "),
        None => String::new(),
    };

    let mut params: Vec<&dyn rusqlite::types::ToSql> = scope_params.to_vec();
    let provider_name = provider.name();
    let provider_dim = provider.dim() as i64;
    params.push(&provider_name);
    params.push(&provider_dim);

    // Refuse the load before decoding anything: any foreign-space row in
    // scope — even alongside compatible rows — is a mixed-transition index,
    // not a searchable one.
    let incompatible_sql = format!(
        "SELECT provider, dim FROM embeddings
         WHERE {scope}(provider != ?{provider_param} OR dim != ?{dim_param})
         LIMIT 1",
    );
    match conn.query_row(&incompatible_sql, params.as_slice(), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    }) {
        Ok((stored_provider, stored_dim)) => {
            return Err(vector_space_mismatch(provider, stored_provider, stored_dim));
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        Err(error) => return Err(EmbeddingError::StorageFailed(error.to_string())),
    }

    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM embeddings
                 WHERE {scope}provider = ?{provider_param} AND dim = ?{dim_param}"
            ),
            params.as_slice(),
            |r| r.get(0),
        )
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;
    let count = count as usize;

    let rows_sql = format!(
        "SELECT symbol_id, vector FROM embeddings
         WHERE {scope}provider = ?{provider_param} AND dim = ?{dim_param}"
    );
    let mut stmt = conn
        .prepare(&rows_sql)
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;
    let rows = stmt
        .query_map(params.as_slice(), |row| {
            let symbol_id: i64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((symbol_id, blob))
        })
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

    let mut results = Vec::with_capacity(count);
    for r in rows {
        let (symbol_id, blob) = r.map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;
        results.push((symbol_id, decode_vector(&blob, provider)?));
    }

    Ok(results)
}

/// Load all embedding vectors from the database.
///
/// Returns `(symbol_id, vector)` pairs.  Uses `bytemuck::try_cast_slice`
/// to validate BLOB alignment, then copies the floats into an owned `Vec<f32>`.
/// (The intermediate `Vec<u8>` is a rusqlite API constraint — SQLite BLOBs
/// must be copied out of the page cache regardless.)
pub fn load_all_embeddings(
    conn: &Connection,
    provider: &dyn EmbeddingProvider,
) -> Result<Vec<(i64, Vec<f32>)>, EmbeddingError> {
    load_scoped_embeddings(conn, provider, None, &[])
}

/// Load embedding vectors for symbols whose file path starts with a prefix.
///
/// Returns `(symbol_id, vector)` pairs, filtered at the SQL level using
/// `WHERE file GLOB 'prefix*' AND NOT stale`.  GLOB is case-sensitive and
/// allows SQLite to use the B-tree index on `embeddings(file)` for prefix
/// patterns.  An empty prefix matches all non-stale embeddings.
///
/// Stale embeddings are excluded because clustering quality depends heavily
/// on vector accuracy — unlike `wonk ask` which includes stale rows as a
/// best-effort fallback.
pub fn load_embeddings_for_path_prefix(
    conn: &Connection,
    prefix: &str,
    provider: &dyn EmbeddingProvider,
) -> Result<Vec<(i64, Vec<f32>)>, EmbeddingError> {
    // Escape GLOB metacharacters (*, ?, [) in user-supplied prefix so only the
    // trailing `*` acts as a wildcard.
    let escaped = prefix
        .replace('[', "[[]")
        .replace('*', "[*]")
        .replace('?', "[?]");
    let pattern = format!("{escaped}*");
    load_scoped_embeddings(
        conn,
        provider,
        Some("file GLOB ?1 AND NOT stale"),
        &[&pattern as &dyn rusqlite::types::ToSql],
    )
}

/// Load embedding vectors only for symbols belonging to the specified files.
///
/// Returns `(symbol_id, vector)` pairs, filtered at the SQL level using
/// `WHERE file IN (...)`.  Returns an empty `Vec` when `files` is empty.
pub fn load_embeddings_for_files(
    conn: &Connection,
    files: &HashSet<String>,
    provider: &dyn EmbeddingProvider,
) -> Result<Vec<(i64, Vec<f32>)>, EmbeddingError> {
    if files.is_empty() {
        return Ok(Vec::new());
    }

    // Build a parameterized IN clause: (?1, ?2, ..., ?N)
    let placeholders: Vec<String> = (1..=files.len()).map(|i| format!("?{i}")).collect();
    let scope = format!("file IN ({})", placeholders.join(", "));
    let file_params: Vec<&dyn rusqlite::types::ToSql> = files
        .iter()
        .map(|f| f as &dyn rusqlite::types::ToSql)
        .collect();
    load_scoped_embeddings(conn, provider, Some(&scope), &file_params)
}

/// Load embedding vectors keyed by the candidate `(file, line)` positions
/// of the symbols they belong to (TASK-093).
///
/// One batched statement: `embeddings JOIN symbols ON id = symbol_id`
/// filtered to the candidate files and the provider's vector space at the
/// SQL level, then position-filtered in Rust. Stale rows are included — a
/// supplementary ranking signal takes a weaker prior over an absence — and
/// rows whose BLOB cannot decode are skipped best-effort rather than
/// failing the load. An empty position set touches no SQL.
pub fn load_embedding_vectors_at_positions(
    conn: &Connection,
    positions: &HashSet<(String, u64)>,
    provider: &dyn EmbeddingProvider,
) -> Result<std::collections::HashMap<(String, u64), Vec<f32>>, EmbeddingError> {
    if positions.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let files: HashSet<&str> = positions.iter().map(|(file, _)| file.as_str()).collect();
    let placeholders: Vec<String> = (1..=files.len()).map(|i| format!("?{i}")).collect();
    let provider_param = files.len() + 1;
    let dim_param = files.len() + 2;
    let sql = format!(
        "SELECT symbols.file, symbols.line, embeddings.vector
         FROM embeddings JOIN symbols ON symbols.id = embeddings.symbol_id
         WHERE symbols.file IN ({}) AND embeddings.provider = ?{} AND embeddings.dim = ?{}",
        placeholders.join(", "),
        provider_param,
        dim_param,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

    let file_params: Vec<&str> = files.into_iter().collect();
    let mut params: Vec<&dyn rusqlite::types::ToSql> = file_params
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let provider_name = provider.name();
    let provider_dim = provider.dim() as i64;
    params.push(&provider_name);
    params.push(&provider_dim);

    let rows = stmt
        .query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

    let mut vectors = std::collections::HashMap::new();
    for row in rows {
        let (file, line, blob) = row.map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;
        let key = (file, line.max(0) as u64);
        if !positions.contains(&key) {
            continue;
        }
        if let Ok(vector) = decode_vector(&blob, provider) {
            vectors.insert(key, vector);
        }
    }
    Ok(vectors)
}

/// Delete all embeddings for a given file.
pub fn delete_embeddings_for_file(conn: &Connection, file: &str) -> Result<(), EmbeddingError> {
    conn.execute(
        "DELETE FROM embeddings WHERE file = ?1",
        rusqlite::params![file],
    )
    .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;
    Ok(())
}

/// Mark all embeddings for a file as stale (`stale = 1`).
pub fn mark_embeddings_stale(conn: &Connection, file: &str) -> Result<(), EmbeddingError> {
    conn.execute(
        "UPDATE embeddings SET stale = 1 WHERE file = ?1",
        rusqlite::params![file],
    )
    .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;
    Ok(())
}

/// Return `(total_symbols, fresh_embedding_count)` for completeness checking.
///
/// `total_symbols` is the count of rows in the `symbols` table.
/// `fresh_embedding_count` is the count of non-stale embeddings.
/// When `total_symbols == fresh_embedding_count`, all symbols are embedded.
pub fn embedding_completeness(
    conn: &Connection,
    provider: &dyn EmbeddingProvider,
) -> Result<(usize, usize), EmbeddingError> {
    let total_symbols: i64 = conn
        .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

    let fresh_embeddings: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM embeddings
             WHERE NOT stale AND provider = ?1 AND dim = ?2",
            rusqlite::params![provider.name(), provider.dim() as i64],
            |row| row.get(0),
        )
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

    Ok((total_symbols as usize, fresh_embeddings as usize))
}

/// Return `(total_count, stale_count)` for embeddings in the database.
pub fn embedding_stats(conn: &Connection) -> Result<(usize, usize), EmbeddingError> {
    let (total, stale): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(stale), 0) FROM embeddings",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

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

/// Shared chunking logic: turn a set of `SymbolRow`s into text chunks.
///
/// Groups symbols by file, reads source from disk, and produces
/// `(symbol_id, file_path, chunk_text)` triples.  Silently skips files
/// that cannot be read or whose paths are absolute / contain `..`.
fn chunk_symbol_rows(
    rows: &[SymbolRow],
    all_imports: &BTreeMap<String, Vec<String>>,
    repo_root: &Path,
) -> Vec<(i64, String, String)> {
    if rows.is_empty() {
        return Vec::new();
    }

    // Group symbols by file (BTreeMap for deterministic iteration order).
    let mut by_file: BTreeMap<String, Vec<&SymbolRow>> = BTreeMap::new();
    for row in rows {
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
            results.push((sym_row.id, file.clone(), chunk));
        }
    }

    results
}

/// Generate text chunks for all indexed symbols.
///
/// Returns `(symbol_id, file_path, chunk_text)` triples.  Reads source files
/// from disk under `repo_root`, and silently skips files that cannot be read
/// or whose paths are absolute or contain `..` components.
pub fn chunk_all_symbols(
    conn: &Connection,
    repo_root: &Path,
) -> Result<Vec<(i64, String, String)>, EmbeddingError> {
    let rows = query_all_symbols(conn)?;
    let all_imports = query_all_file_imports(conn)?;
    Ok(chunk_symbol_rows(&rows, &all_imports, repo_root))
}

/// Generate text chunks for symbols that lack fresh (non-stale) embeddings.
///
/// Like [`chunk_all_symbols`] but only processes symbols returned by
/// [`query_unembedded_symbols`].
pub fn chunk_missing_symbols(
    conn: &Connection,
    repo_root: &Path,
    provider: &dyn EmbeddingProvider,
) -> Result<Vec<(i64, String, String)>, EmbeddingError> {
    let rows = query_unembedded_symbols(conn, provider)?;
    let all_imports = query_all_file_imports(conn)?;
    Ok(chunk_symbol_rows(&rows, &all_imports, repo_root))
}

/// Generate text chunks for symbols belonging to specific files.
///
/// Like [`chunk_all_symbols`] but only processes symbols whose `file` column
/// matches one of the provided paths.  Returns an empty `Vec` when `files`
/// is empty.
pub fn chunk_symbols_for_files(
    conn: &Connection,
    repo_root: &Path,
    files: &[String],
) -> Result<Vec<(i64, String, String)>, EmbeddingError> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let rows = query_symbols_for_files(conn, files)?;
    let all_imports = query_file_imports_for_files(conn, files)?;
    Ok(chunk_symbol_rows(&rows, &all_imports, repo_root))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TinyProvider;
    struct OneDimProvider;

    impl EmbeddingProvider for TinyProvider {
        fn name(&self) -> &str {
            "tiny"
        }

        fn dim(&self) -> usize {
            2
        }

        fn embed_batch(&self, chunks: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Ok(chunks.iter().map(|_| vec![3.0, 4.0]).collect())
        }
    }

    impl EmbeddingProvider for OneDimProvider {
        fn name(&self) -> &str {
            "test"
        }

        fn dim(&self) -> usize {
            1
        }

        fn embed_batch(&self, chunks: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Ok(chunks.iter().map(|_| vec![1.0]).collect())
        }
    }

    fn insert_test_symbol(conn: &Connection, name: &str, file: &str) -> i64 {
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language)
             VALUES (?1, 'function', ?2, 1, 0, 'rust')",
            rusqlite::params![name, file],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn provider_trait_dispatches_and_supplies_single_helper() {
        let provider: &dyn EmbeddingProvider = &TinyProvider;
        assert_eq!(provider.name(), "tiny");
        assert_eq!(provider.dim(), 2);
        assert_eq!(provider.embed_single("query").unwrap(), vec![3.0, 4.0]);
    }

    #[test]
    fn ollama_provider_declares_legacy_vector_space() {
        let provider = OllamaProvider::new();
        assert_eq!(provider.name(), "ollama");
        assert_eq!(provider.dim(), OLLAMA_DIM);
        assert_eq!(provider.dim(), 768);
    }

    #[test]
    fn bundled_provider_is_available_with_stable_metadata() {
        let provider = create_provider(EmbeddingProviderKind::Bundled)
            .expect("the bundled provider should always be available");
        assert_eq!(provider.name(), "bundled");
        assert_eq!(provider.dim(), 256);
    }

    #[test]
    fn invocation_provider_override_wins_over_configuration() {
        assert_eq!(
            resolve_provider_kind(
                Some(EmbeddingProviderKind::Ollama),
                EmbeddingProviderKind::Bundled,
            ),
            EmbeddingProviderKind::Ollama
        );
        assert_eq!(
            resolve_provider_kind(None, EmbeddingProviderKind::Ollama),
            EmbeddingProviderKind::Ollama
        );
    }

    #[test]
    fn provider_kind_parse_accepts_known_names_and_rejects_others() {
        assert_eq!(
            EmbeddingProviderKind::parse("bundled").unwrap(),
            EmbeddingProviderKind::Bundled
        );
        assert_eq!(
            EmbeddingProviderKind::parse("ollama").unwrap(),
            EmbeddingProviderKind::Ollama
        );
        assert_eq!(
            EmbeddingProviderKind::parse("remote").unwrap_err(),
            "invalid embedding provider: remote"
        );
    }

    #[test]
    fn provider_metadata_round_trips_and_wrong_dimension_is_rejected() {
        let conn = setup_test_db_with_embeddings();
        let symbol_id = insert_test_symbol(&conn, "tiny", "tiny.rs");

        let mismatch = store_embedding(&conn, &TinyProvider, symbol_id, "tiny.rs", "chunk", &[1.0])
            .unwrap_err();
        assert!(matches!(
            mismatch,
            EmbeddingError::VectorDimension {
                expected: 2,
                actual: 1,
                ..
            }
        ));

        store_embedding(
            &conn,
            &TinyProvider,
            symbol_id,
            "tiny.rs",
            "chunk",
            &[3.0, 4.0],
        )
        .unwrap();

        let metadata: (String, i64) = conn
            .query_row(
                "SELECT provider, dim FROM embeddings WHERE symbol_id = ?1",
                [symbol_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(metadata, ("tiny".to_string(), 2));
        assert_eq!(
            load_all_embeddings(&conn, &TinyProvider).unwrap(),
            vec![(symbol_id, vec![0.6, 0.8])]
        );
    }

    #[test]
    fn incompatible_space_fails_with_reembed_instruction() {
        let conn = setup_test_db_with_embeddings();
        let symbol_id = insert_test_symbol(&conn, "legacy", "legacy.rs");
        conn.execute(
            "INSERT INTO embeddings
                (symbol_id, file, chunk_text, vector, stale, created_at, provider, dim)
             VALUES (?1, 'legacy.rs', 'chunk', ?2, 0, 0, 'ollama', 768)",
            rusqlite::params![symbol_id, bytemuck::cast_slice(&[1.0_f32, 0.0])],
        )
        .unwrap();

        let error = load_all_embeddings(&conn, &TinyProvider).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("active tiny/2"));
        assert!(message.contains("stored ollama/768"));
        assert!(message.contains("wonk update --force --provider tiny"));
    }

    #[test]
    fn partial_partition_fails_fast_with_mismatch() {
        // A partially migrated index (some rows re-embedded with the active
        // provider, some still foreign) must refuse to serve a partial
        // semantic search: any incompatible row in scope fails with the
        // re-embed instruction, whether or not compatible rows also exist.
        let conn = setup_test_db_with_embeddings();
        let tiny_id = insert_test_symbol(&conn, "tiny", "tiny.rs");
        let ollama_id = insert_test_symbol(&conn, "legacy", "legacy.rs");
        store_embedding(
            &conn,
            &TinyProvider,
            tiny_id,
            "tiny.rs",
            "chunk",
            &[1.0, 0.0],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO embeddings
                (symbol_id, file, chunk_text, vector, stale, created_at, provider, dim)
             VALUES (?1, 'legacy.rs', 'chunk', ?2, 0, 0, 'ollama', 768)",
            rusqlite::params![ollama_id, bytemuck::cast_slice(&[0.0_f32, 1.0])],
        )
        .unwrap();

        let error = load_all_embeddings(&conn, &TinyProvider).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("active tiny/2"), "got: {message}");
        assert!(message.contains("stored ollama/768"), "got: {message}");
        assert!(
            message.contains("wonk update --force --provider tiny"),
            "got: {message}"
        );
    }

    #[test]
    fn default_client_uses_localhost() {
        let client = OllamaProvider::new();
        assert_eq!(client.base_url, DEFAULT_BASE_URL);
        assert_eq!(client.model, DEFAULT_MODEL);
    }

    #[test]
    fn with_base_url_trims_trailing_slash() {
        let client = OllamaProvider::with_base_url("http://example.com:11434/");
        assert_eq!(client.base_url, "http://example.com:11434");
    }

    #[test]
    fn with_base_url_preserves_clean_url() {
        let client = OllamaProvider::with_base_url("http://example.com:11434");
        assert_eq!(client.base_url, "http://example.com:11434");
    }

    // -- Health check tests ---------------------------------------------------

    #[test]
    fn health_check_returns_false_when_unreachable() {
        // Port 19999 should have nothing listening.
        let client = OllamaProvider::with_base_url("http://127.0.0.1:19999");
        assert!(!client.is_healthy());
    }

    #[test]
    fn quick_health_probe_is_bounded_when_server_stalls() {
        // A wedged server that accepts the TCP connection but never returns
        // response headers must not stall the probe: the whole request —
        // connect, headers — is bounded, not just the connect phase.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    // Hold each connection open without ever responding.
                    Ok(_stream) => std::thread::sleep(Duration::from_secs(30)),
                    Err(_) => break,
                }
            }
        });

        let client = OllamaProvider::with_base_url(&format!("http://{addr}"));
        let start = std::time::Instant::now();
        assert!(!client.is_healthy_quick());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "probe must be bounded end-to-end, took {:?}",
            start.elapsed()
        );
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
        let client = OllamaProvider::with_base_url("http://127.0.0.1:19999");
        let result = client.embed_batch(&[]);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn embed_batch_rejects_oversized_input() {
        let client = OllamaProvider::with_base_url("http://127.0.0.1:19999");
        let oversized = "x".repeat(32_769);
        let texts = vec![oversized];
        let result = client.embed_batch(&texts);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            EmbeddingError::OllamaError(msg) => {
                assert!(msg.contains("too long"), "expected 'too long' in: {msg}");
                assert!(msg.contains("32769"), "expected byte count in: {msg}");
            }
            other => panic!("expected OllamaError, got: {other:?}"),
        }
    }

    #[test]
    fn embed_batch_unreachable_returns_error() {
        let client = OllamaProvider::with_base_url("http://127.0.0.1:19999");
        let texts = vec!["hello".to_string()];
        let result = client.embed_batch(&texts);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(
            matches!(error, EmbeddingError::OllamaUnreachable),
            "unexpected error: {error:?}"
        );
    }

    // -- is_context_length_error tests ----------------------------------------

    #[test]
    fn context_length_error_matches_server_message() {
        let err =
            EmbeddingError::OllamaError("the input length exceeds the context length".to_string());
        assert!(is_context_length_error(&err));
    }

    #[test]
    fn context_length_error_matches_client_preflight() {
        let err =
            EmbeddingError::OllamaError("input text too long (40000 bytes, max 32768)".to_string());
        assert!(is_context_length_error(&err));
    }

    #[test]
    fn context_length_error_rejects_unrelated_ollama_error() {
        let err = EmbeddingError::OllamaError("model not found".to_string());
        assert!(!is_context_length_error(&err));
    }

    #[test]
    fn context_length_error_rejects_unreachable() {
        assert!(!is_context_length_error(&EmbeddingError::OllamaUnreachable));
    }

    #[test]
    fn context_length_error_rejects_invalid_response() {
        assert!(!is_context_length_error(&EmbeddingError::InvalidResponse));
    }

    // -- embed_single tests ---------------------------------------------------

    #[test]
    fn embed_single_unreachable_returns_error() {
        let client = OllamaProvider::with_base_url("http://127.0.0.1:19999");
        let result = client.embed_single("hello");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(
            matches!(error, EmbeddingError::OllamaUnreachable),
            "unexpected error: {error:?}"
        );
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
            doc_comment: None,
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

    #[allow(clippy::too_many_arguments)]
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
        // (symbol_id, file_path, chunk_text)
        assert_eq!(chunks[0].0, sym_id);
        assert_eq!(chunks[0].1, "main.rs");
        assert!(chunks[0].2.contains("File: main.rs"));
        assert!(chunks[0].2.contains("Imports: std::io"));
        assert!(chunks[0].2.contains("fn hello()"));
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
        let ids: Vec<i64> = chunks.iter().map(|(id, _, _)| *id).collect();
        assert!(ids.contains(&id_a));
        assert!(ids.contains(&id_b));
        // All chunks from same file should have same file_path
        assert!(chunks.iter().all(|(_, file, _)| file == "lib.rs"));
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
                provider TEXT NOT NULL DEFAULT 'ollama',
                dim INTEGER NOT NULL DEFAULT 768,
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
        store_embedding(&conn, &TinyProvider, sym_id, "a.rs", "fn foo() {}", &vector).unwrap();

        let loaded = load_all_embeddings(&conn, &TinyProvider).unwrap();
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
        store_embedding(&conn, &TinyProvider, sym_id, "a.rs", "v1", &v1).unwrap();

        let v2 = vec![0.0_f32, 1.0];
        store_embedding(&conn, &TinyProvider, sym_id, "a.rs", "v2", &v2).unwrap();

        let loaded = load_all_embeddings(&conn, &TinyProvider).unwrap();
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
        store_embeddings_batch(&conn, &TinyProvider, &batch).unwrap();

        let loaded = load_all_embeddings(&conn, &TinyProvider).unwrap();
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
        let result = store_embeddings_batch(&conn, &TinyProvider, &batch);
        assert!(result.is_err());

        // Atomic: nothing should have been inserted.
        let loaded = load_all_embeddings(&conn, &TinyProvider).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_all_embeddings_empty_db() {
        let conn = setup_test_db_with_embeddings();
        let loaded = load_all_embeddings(&conn, &TinyProvider).unwrap();
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

        store_embedding(&conn, &TinyProvider, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        store_embedding(&conn, &TinyProvider, id2, "b.rs", "fn b()", &[0.0, 1.0]).unwrap();

        delete_embeddings_for_file(&conn, "a.rs").unwrap();

        let loaded = load_all_embeddings(&conn, &TinyProvider).unwrap();
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

        store_embedding(&conn, &TinyProvider, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        store_embedding(&conn, &TinyProvider, id2, "b.rs", "fn b()", &[0.0, 1.0]).unwrap();

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

        store_embedding(&conn, &TinyProvider, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        store_embedding(&conn, &TinyProvider, id2, "a.rs", "fn b()", &[0.0, 1.0]).unwrap();
        store_embedding(&conn, &TinyProvider, id3, "b.rs", "fn c()", &[0.7, 0.7]).unwrap();

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

    // -- embedding_completeness tests -----------------------------------------

    #[test]
    fn embedding_completeness_no_symbols_no_embeddings() {
        let conn = setup_test_db_with_embeddings();
        let (sym_count, emb_count) = embedding_completeness(&conn, &TinyProvider).unwrap();
        assert_eq!(sym_count, 0);
        assert_eq!(emb_count, 0);
    }

    #[test]
    fn embedding_completeness_symbols_no_embeddings() {
        let conn = setup_test_db_with_embeddings();
        insert_symbol(
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
        insert_symbol(
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
        let (sym_count, emb_count) = embedding_completeness(&conn, &TinyProvider).unwrap();
        assert_eq!(sym_count, 2);
        assert_eq!(emb_count, 0);
    }

    #[test]
    fn embedding_completeness_partial() {
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
        insert_symbol(
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
        store_embedding(&conn, &TinyProvider, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        let (sym_count, emb_count) = embedding_completeness(&conn, &TinyProvider).unwrap();
        assert_eq!(sym_count, 2);
        assert_eq!(emb_count, 1);
    }

    #[test]
    fn embedding_completeness_all_embedded() {
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
        store_embedding(&conn, &TinyProvider, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        store_embedding(&conn, &TinyProvider, id2, "b.rs", "fn b()", &[0.0, 1.0]).unwrap();
        let (sym_count, emb_count) = embedding_completeness(&conn, &TinyProvider).unwrap();
        assert_eq!(sym_count, 2);
        assert_eq!(emb_count, 2);
    }

    #[test]
    fn embedding_completeness_excludes_stale() {
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
        store_embedding(&conn, &TinyProvider, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        store_embedding(&conn, &TinyProvider, id2, "b.rs", "fn b()", &[0.0, 1.0]).unwrap();
        // Mark one as stale -- should not count as fresh
        mark_embeddings_stale(&conn, "a.rs").unwrap();
        let (sym_count, emb_count) = embedding_completeness(&conn, &TinyProvider).unwrap();
        assert_eq!(sym_count, 2);
        assert_eq!(emb_count, 1);
    }

    // -- query_unembedded_symbols tests ---------------------------------------

    #[test]
    fn query_unembedded_symbols_all_missing() {
        let conn = setup_test_db_with_embeddings();
        insert_symbol(
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
        insert_symbol(
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
        let rows = query_unembedded_symbols(&conn, &TinyProvider).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn query_unembedded_symbols_some_embedded() {
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
        insert_symbol(
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
        store_embedding(&conn, &TinyProvider, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        let rows = query_unembedded_symbols(&conn, &TinyProvider).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol.name, "b");
    }

    #[test]
    fn query_unembedded_symbols_all_embedded() {
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
        store_embedding(&conn, &TinyProvider, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        store_embedding(&conn, &TinyProvider, id2, "b.rs", "fn b()", &[0.0, 1.0]).unwrap();
        let rows = query_unembedded_symbols(&conn, &TinyProvider).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn query_unembedded_symbols_stale_reembedded() {
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
        store_embedding(&conn, &TinyProvider, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        store_embedding(&conn, &TinyProvider, id2, "b.rs", "fn b()", &[0.0, 1.0]).unwrap();
        // Mark id1's embedding as stale -- it needs re-embedding
        mark_embeddings_stale(&conn, "a.rs").unwrap();
        let rows = query_unembedded_symbols(&conn, &TinyProvider).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol.name, "a");
    }

    // -- chunk_missing_symbols tests ------------------------------------------

    #[test]
    fn chunk_missing_symbols_all_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "fn a() { 1 }\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() { 2 }\n").unwrap();

        let conn = setup_test_db_with_embeddings();
        insert_symbol(
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
        insert_symbol(
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

        let chunks = chunk_missing_symbols(&conn, root, &TinyProvider).unwrap();
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn chunk_missing_symbols_some_embedded() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "fn a() { 1 }\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() { 2 }\n").unwrap();

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
        insert_symbol(
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
        store_embedding(&conn, &TinyProvider, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();

        let chunks = chunk_missing_symbols(&conn, root, &TinyProvider).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, "b.rs");
    }

    #[test]
    fn chunk_missing_symbols_all_embedded() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "fn a() { 1 }\n").unwrap();

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
        store_embedding(&conn, &TinyProvider, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();

        let chunks = chunk_missing_symbols(&conn, root, &TinyProvider).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_missing_symbols_stale_gets_rechunked() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "fn a() { 1 }\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() { 2 }\n").unwrap();

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
        store_embedding(&conn, &TinyProvider, id1, "a.rs", "fn a()", &[1.0, 0.0]).unwrap();
        store_embedding(&conn, &TinyProvider, id2, "b.rs", "fn b()", &[0.0, 1.0]).unwrap();
        mark_embeddings_stale(&conn, "a.rs").unwrap();

        let chunks = chunk_missing_symbols(&conn, root, &TinyProvider).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, "a.rs");
    }

    // -- chunk_symbols_for_files tests ----------------------------------------

    #[test]
    fn chunk_symbols_for_files_targets_specific_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "fn a() { 1 }\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() { 2 }\n").unwrap();
        std::fs::write(root.join("c.rs"), "fn c() { 3 }\n").unwrap();

        let conn = setup_test_db();
        insert_symbol(
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
        insert_symbol(
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
        insert_symbol(
            &conn,
            "c",
            "function",
            "c.rs",
            1,
            Some(1),
            None,
            "fn c()",
            "Rust",
        );

        // Only chunk symbols for a.rs and c.rs.
        let files = vec!["a.rs".to_string(), "c.rs".to_string()];
        let chunks = chunk_symbols_for_files(&conn, root, &files).unwrap();
        assert_eq!(chunks.len(), 2);
        let file_paths: Vec<&str> = chunks.iter().map(|(_, f, _)| f.as_str()).collect();
        assert!(file_paths.contains(&"a.rs"));
        assert!(file_paths.contains(&"c.rs"));
        assert!(!file_paths.contains(&"b.rs"));
    }

    #[test]
    fn chunk_symbols_for_files_empty_list_returns_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "fn a() { 1 }\n").unwrap();

        let conn = setup_test_db();
        insert_symbol(
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

        let files: Vec<String> = vec![];
        let chunks = chunk_symbols_for_files(&conn, root, &files).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_symbols_for_files_nonexistent_file_skipped() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        // a.rs exists on disk and in DB, ghost.rs exists in DB but not on disk.
        std::fs::write(root.join("a.rs"), "fn a() { 1 }\n").unwrap();

        let conn = setup_test_db();
        insert_symbol(
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
        insert_symbol(
            &conn,
            "ghost",
            "function",
            "ghost.rs",
            1,
            Some(1),
            None,
            "fn ghost()",
            "Rust",
        );

        let files = vec!["a.rs".to_string(), "ghost.rs".to_string()];
        let chunks = chunk_symbols_for_files(&conn, root, &files).unwrap();
        // Only a.rs should produce chunks (ghost.rs can't be read from disk).
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, "a.rs");
    }

    // -- load_embeddings_for_files tests --------------------------------------

    fn setup_db_with_embeddings() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE symbols (
                id INTEGER PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL,
                file TEXT NOT NULL, line INTEGER NOT NULL, col INTEGER NOT NULL,
                end_line INTEGER, scope TEXT, signature TEXT, language TEXT NOT NULL
            );
            CREATE TABLE embeddings (
                id INTEGER PRIMARY KEY,
                symbol_id INTEGER NOT NULL REFERENCES symbols(id),
                file TEXT NOT NULL, chunk_text TEXT NOT NULL, vector BLOB NOT NULL,
                stale INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
                provider TEXT NOT NULL DEFAULT 'test', dim INTEGER NOT NULL DEFAULT 1,
                UNIQUE(symbol_id)
            );
            CREATE INDEX idx_embeddings_file ON embeddings(file);",
        )
        .unwrap();
        conn
    }

    fn insert_symbol_and_embedding(conn: &Connection, sym_id: i64, file: &str, vec_bytes: &[u8]) {
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) \
             VALUES (?1, ?2, 'function', ?3, 1, 0, 'rust')",
            rusqlite::params![sym_id, format!("sym_{}", sym_id), file],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, created_at) \
             VALUES (?1, ?2, 'chunk', ?3, 1000)",
            rusqlite::params![sym_id, file, vec_bytes],
        )
        .unwrap();
    }

    #[test]
    fn test_load_embeddings_for_files_filters_correctly() {
        let conn = setup_db_with_embeddings();
        // 4 bytes = 1 f32
        let vec_a: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        let vec_b: Vec<u8> = bytemuck::cast_slice(&[2.0_f32]).to_vec();
        let vec_c: Vec<u8> = bytemuck::cast_slice(&[3.0_f32]).to_vec();

        insert_symbol_and_embedding(&conn, 1, "src/a.ts", &vec_a);
        insert_symbol_and_embedding(&conn, 2, "src/b.ts", &vec_b);
        insert_symbol_and_embedding(&conn, 3, "src/c.ts", &vec_c);

        let files: HashSet<String> = ["src/a.ts", "src/c.ts"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let results = load_embeddings_for_files(&conn, &files, &OneDimProvider).unwrap();

        assert_eq!(results.len(), 2);
        let ids: HashSet<i64> = results.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&3));
        assert!(!ids.contains(&2));
    }

    #[test]
    fn test_load_embeddings_for_files_empty_set() {
        let conn = setup_db_with_embeddings();
        let vec_a: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        insert_symbol_and_embedding(&conn, 1, "src/a.ts", &vec_a);

        let files: HashSet<String> = HashSet::new();
        let results = load_embeddings_for_files(&conn, &files, &OneDimProvider).unwrap();
        assert!(results.is_empty());
    }

    // -- load_embedding_vectors_at_positions tests (TASK-093) -----------------

    /// Insert a symbol at an explicit line plus its embedding row.
    fn insert_symbol_at_line_with_embedding(
        conn: &Connection,
        sym_id: i64,
        file: &str,
        line: i64,
        vec_bytes: &[u8],
    ) {
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) \
             VALUES (?1, ?2, 'function', ?3, ?4, 0, 'rust')",
            rusqlite::params![sym_id, format!("sym_{sym_id}"), file, line],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, created_at) \
             VALUES (?1, ?2, 'chunk', ?3, 1000)",
            rusqlite::params![sym_id, file, vec_bytes],
        )
        .unwrap();
    }

    fn position_set(entries: &[(&str, u64)]) -> HashSet<(String, u64)> {
        entries.iter().map(|(f, l)| (f.to_string(), *l)).collect()
    }

    #[test]
    fn load_vectors_at_positions_filters_to_candidate_positions() {
        let conn = setup_db_with_embeddings();
        // gamma at src/a.ts:5, delta at src/a.ts:9, eps at src/b.ts:3.
        let vec: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        insert_symbol_at_line_with_embedding(&conn, 1, "src/a.ts", 5, &vec);
        insert_symbol_at_line_with_embedding(&conn, 2, "src/a.ts", 9, &vec);
        insert_symbol_at_line_with_embedding(&conn, 3, "src/b.ts", 3, &vec);

        // Candidates are the gamma and eps positions only.
        let positions = position_set(&[("src/a.ts", 5), ("src/b.ts", 3)]);
        let loaded =
            load_embedding_vectors_at_positions(&conn, &positions, &OneDimProvider).unwrap();

        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains_key(&("src/a.ts".to_string(), 5)));
        assert!(loaded.contains_key(&("src/b.ts".to_string(), 3)));
        // The delta definition shares the file but not the line.
        assert!(!loaded.contains_key(&("src/a.ts".to_string(), 9)));
    }

    #[test]
    fn load_vectors_at_positions_excludes_wrong_provider_or_dim() {
        let conn = setup_db_with_embeddings();
        let vec: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        insert_symbol_at_line_with_embedding(&conn, 1, "src/a.ts", 5, &vec);
        insert_symbol_at_line_with_embedding(&conn, 2, "src/b.ts", 3, &vec);
        // b.ts lives in a foreign vector space.
        conn.execute(
            "UPDATE embeddings SET provider = 'ollama', dim = 768 WHERE symbol_id = 2",
            [],
        )
        .unwrap();

        let positions = position_set(&[("src/a.ts", 5), ("src/b.ts", 3)]);
        let loaded =
            load_embedding_vectors_at_positions(&conn, &positions, &OneDimProvider).unwrap();

        assert_eq!(loaded.len(), 1, "foreign-space rows are invisible");
        assert!(loaded.contains_key(&("src/a.ts".to_string(), 5)));
    }

    #[test]
    fn load_vectors_at_positions_includes_stale_rows() {
        let conn = setup_db_with_embeddings();
        let vec: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        insert_symbol_at_line_with_embedding(&conn, 1, "src/a.ts", 5, &vec);
        conn.execute("UPDATE embeddings SET stale = 1 WHERE symbol_id = 1", [])
            .unwrap();

        let positions = position_set(&[("src/a.ts", 5)]);
        let loaded =
            load_embedding_vectors_at_positions(&conn, &positions, &OneDimProvider).unwrap();
        // Best-effort: a stale vector is a weaker prior, not an absence.
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn load_vectors_at_positions_skips_corrupt_blobs_best_effort() {
        let conn = setup_db_with_embeddings();
        let good: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        insert_symbol_at_line_with_embedding(&conn, 1, "src/a.ts", 5, &good);
        // A BLOB whose byte length is not dim * 4 cannot decode.
        insert_symbol_at_line_with_embedding(&conn, 2, "src/b.ts", 3, &[1, 2, 3]);

        let positions = position_set(&[("src/a.ts", 5), ("src/b.ts", 3)]);
        let loaded =
            load_embedding_vectors_at_positions(&conn, &positions, &OneDimProvider).unwrap();

        assert_eq!(loaded.len(), 1, "corrupt row skipped, healthy row kept");
        assert!(loaded.contains_key(&("src/a.ts".to_string(), 5)));
    }

    #[test]
    fn load_vectors_at_positions_empty_positions_yield_empty_map() {
        let conn = setup_db_with_embeddings();
        let vec: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        insert_symbol_at_line_with_embedding(&conn, 1, "src/a.ts", 5, &vec);
        let loaded =
            load_embedding_vectors_at_positions(&conn, &HashSet::new(), &OneDimProvider).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_load_embeddings_for_files_rejects_mismatched_vector_space() {
        let conn = setup_db_with_embeddings();
        let vector: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        insert_symbol_and_embedding(&conn, 1, "src/a.ts", &vector);
        conn.execute(
            "UPDATE embeddings SET provider = 'ollama', dim = 768 WHERE symbol_id = 1",
            [],
        )
        .unwrap();

        let files = ["src/a.ts".to_string()].into_iter().collect();
        let error = load_embeddings_for_files(&conn, &files, &OneDimProvider).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("active test/1"));
        assert!(message.contains("stored ollama/768"));
        assert!(message.contains("wonk update --force --provider test"));
    }

    #[test]
    fn test_load_embeddings_for_files_partial_partition_fails_fast() {
        // One compatible row plus one foreign-space row in the requested
        // files: the load must fail with the mismatch, not return the
        // compatible subset.
        let conn = setup_db_with_embeddings();
        let compatible: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        let foreign: Vec<u8> = bytemuck::cast_slice(&[2.0_f32, 768.0]).to_vec();
        insert_symbol_and_embedding(&conn, 1, "src/a.ts", &compatible);
        insert_symbol_and_embedding(&conn, 2, "src/b.ts", &foreign);
        conn.execute(
            "UPDATE embeddings SET provider = 'ollama', dim = 768 WHERE symbol_id = 2",
            [],
        )
        .unwrap();

        let files = ["src/a.ts".to_string(), "src/b.ts".to_string()]
            .into_iter()
            .collect();
        let error = load_embeddings_for_files(&conn, &files, &OneDimProvider).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("active test/1"), "got: {message}");
        assert!(message.contains("stored ollama/768"), "got: {message}");
    }

    // -- load_embeddings_for_path_prefix tests --------------------------------

    fn insert_symbol_and_embedding_stale(
        conn: &Connection,
        sym_id: i64,
        file: &str,
        vec_bytes: &[u8],
    ) {
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) \
             VALUES (?1, ?2, 'function', ?3, 1, 0, 'rust')",
            rusqlite::params![sym_id, format!("sym_{}", sym_id), file],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, stale, created_at) \
             VALUES (?1, ?2, 'chunk', ?3, 1, 1000)",
            rusqlite::params![sym_id, file, vec_bytes],
        )
        .unwrap();
    }

    #[test]
    fn test_load_embeddings_for_path_prefix_filters_correctly() {
        let conn = setup_db_with_embeddings();
        let vec_a: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        let vec_b: Vec<u8> = bytemuck::cast_slice(&[2.0_f32]).to_vec();
        let vec_c: Vec<u8> = bytemuck::cast_slice(&[3.0_f32]).to_vec();

        insert_symbol_and_embedding(&conn, 1, "src/auth/middleware.ts", &vec_a);
        insert_symbol_and_embedding(&conn, 2, "src/auth/session.ts", &vec_b);
        insert_symbol_and_embedding(&conn, 3, "src/db/connection.ts", &vec_c);

        let results = load_embeddings_for_path_prefix(&conn, "src/auth/", &OneDimProvider).unwrap();
        assert_eq!(results.len(), 2);
        let ids: HashSet<i64> = results.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(!ids.contains(&3));
    }

    #[test]
    fn test_load_embeddings_for_path_prefix_empty_result() {
        let conn = setup_db_with_embeddings();
        let vec_a: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        insert_symbol_and_embedding(&conn, 1, "src/auth/middleware.ts", &vec_a);

        let results = load_embeddings_for_path_prefix(&conn, "lib/", &OneDimProvider).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_load_embeddings_for_path_prefix_rejects_mismatched_vector_space() {
        let conn = setup_db_with_embeddings();
        let vector: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        insert_symbol_and_embedding(&conn, 1, "src/auth/middleware.ts", &vector);
        conn.execute(
            "UPDATE embeddings SET provider = 'ollama', dim = 768 WHERE symbol_id = 1",
            [],
        )
        .unwrap();

        let error =
            load_embeddings_for_path_prefix(&conn, "src/auth/", &OneDimProvider).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("active test/1"));
        assert!(message.contains("stored ollama/768"));
        assert!(message.contains("wonk update --force --provider test"));
    }

    #[test]
    fn test_load_embeddings_for_path_prefix_partial_partition_fails_fast() {
        // A partially migrated prefix scope: one compatible row plus one
        // foreign-space row under the same prefix must fail, not silently
        // drop the foreign row from the clustering input.
        let conn = setup_db_with_embeddings();
        let compatible: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        let foreign: Vec<u8> = bytemuck::cast_slice(&[2.0_f32]).to_vec();
        insert_symbol_and_embedding(&conn, 1, "src/auth/middleware.ts", &compatible);
        insert_symbol_and_embedding(&conn, 2, "src/auth/session.ts", &foreign);
        conn.execute(
            "UPDATE embeddings SET provider = 'ollama', dim = 768 WHERE symbol_id = 2",
            [],
        )
        .unwrap();

        let error =
            load_embeddings_for_path_prefix(&conn, "src/auth/", &OneDimProvider).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("active test/1"), "got: {message}");
        assert!(message.contains("stored ollama/768"), "got: {message}");
    }

    #[test]
    fn test_load_embeddings_for_path_prefix_all_match() {
        let conn = setup_db_with_embeddings();
        let vec_a: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        let vec_b: Vec<u8> = bytemuck::cast_slice(&[2.0_f32]).to_vec();

        insert_symbol_and_embedding(&conn, 1, "src/auth/middleware.ts", &vec_a);
        insert_symbol_and_embedding(&conn, 2, "src/auth/session.ts", &vec_b);

        let results = load_embeddings_for_path_prefix(&conn, "src/", &OneDimProvider).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_load_embeddings_for_path_prefix_excludes_stale() {
        let conn = setup_db_with_embeddings();
        let vec_a: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        let vec_b: Vec<u8> = bytemuck::cast_slice(&[2.0_f32]).to_vec();

        insert_symbol_and_embedding(&conn, 1, "src/auth/middleware.ts", &vec_a);
        insert_symbol_and_embedding_stale(&conn, 2, "src/auth/session.ts", &vec_b);

        let results = load_embeddings_for_path_prefix(&conn, "src/auth/", &OneDimProvider).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn test_load_embeddings_for_path_prefix_empty_prefix_matches_all() {
        let conn = setup_db_with_embeddings();
        let vec_a: Vec<u8> = bytemuck::cast_slice(&[1.0_f32]).to_vec();
        let vec_b: Vec<u8> = bytemuck::cast_slice(&[2.0_f32]).to_vec();

        insert_symbol_and_embedding(&conn, 1, "src/auth/middleware.ts", &vec_a);
        insert_symbol_and_embedding(&conn, 2, "lib/utils.ts", &vec_b);

        let results = load_embeddings_for_path_prefix(&conn, "", &OneDimProvider).unwrap();
        assert_eq!(results.len(), 2);
    }

    // -- decide_query_provider decision table ----------------------------------

    use super::{QueryProviderDecision, StoredVectorSpace, decide_query_provider};

    fn space(provider: &str, dim: usize, rows: usize) -> StoredVectorSpace {
        StoredVectorSpace {
            provider: provider.to_string(),
            dim,
            rows,
        }
    }

    const BUNDLED: (&str, usize) = ("bundled", 256);
    const OLLAMA: (&str, usize) = ("ollama", 768);

    #[test]
    fn decide_bundled_with_empty_table_is_active() {
        let decision = decide_query_provider(EmbeddingProviderKind::Bundled, true, &[]);
        assert_eq!(decision, QueryProviderDecision::Active);
    }

    #[test]
    fn decide_bundled_with_stored_bundled_rows_is_active() {
        let stored = [space(BUNDLED.0, BUNDLED.1, 10)];
        let decision = decide_query_provider(EmbeddingProviderKind::Bundled, true, &stored);
        assert_eq!(decision, QueryProviderDecision::Active);
    }

    #[test]
    fn decide_bundled_with_mixed_rows_still_uses_bundled() {
        let stored = [space(BUNDLED.0, BUNDLED.1, 3), space(OLLAMA.0, OLLAMA.1, 7)];
        let decision = decide_query_provider(EmbeddingProviderKind::Bundled, true, &stored);
        assert_eq!(decision, QueryProviderDecision::Active);
    }

    #[test]
    fn decide_bundled_with_only_foreign_rows_blocks() {
        let stored = [space(OLLAMA.0, OLLAMA.1, 9)];
        let decision = decide_query_provider(EmbeddingProviderKind::Bundled, true, &stored);
        assert_eq!(
            decision,
            QueryProviderDecision::Block {
                active_provider: "bundled".to_string(),
                active_dim: 256,
                stored_provider: "ollama".to_string(),
                stored_dim: 768,
            }
        );
    }

    #[test]
    fn decide_ollama_healthy_with_empty_table_is_active() {
        let decision = decide_query_provider(EmbeddingProviderKind::Ollama, true, &[]);
        assert_eq!(decision, QueryProviderDecision::Active);
    }

    #[test]
    fn decide_ollama_healthy_with_stored_ollama_rows_is_active() {
        let stored = [space(OLLAMA.0, OLLAMA.1, 5)];
        let decision = decide_query_provider(EmbeddingProviderKind::Ollama, true, &stored);
        assert_eq!(decision, QueryProviderDecision::Active);
    }

    #[test]
    fn decide_ollama_healthy_after_switch_to_bundled_index_blocks() {
        let stored = [space(BUNDLED.0, BUNDLED.1, 5)];
        let decision = decide_query_provider(EmbeddingProviderKind::Ollama, true, &stored);
        assert_eq!(
            decision,
            QueryProviderDecision::Block {
                active_provider: "ollama".to_string(),
                active_dim: 768,
                stored_provider: "bundled".to_string(),
                stored_dim: 256,
            }
        );
    }

    #[test]
    fn decide_ollama_unreachable_with_empty_table_falls_back() {
        let decision = decide_query_provider(EmbeddingProviderKind::Ollama, false, &[]);
        assert_eq!(decision, QueryProviderDecision::BundledFallback);
    }

    #[test]
    fn decide_ollama_unreachable_with_stored_bundled_rows_falls_back() {
        let stored = [space(BUNDLED.0, BUNDLED.1, 12)];
        let decision = decide_query_provider(EmbeddingProviderKind::Ollama, false, &stored);
        assert_eq!(decision, QueryProviderDecision::BundledFallback);
    }

    #[test]
    fn decide_ollama_unreachable_with_only_ollama_rows_blocks() {
        let stored = [space(OLLAMA.0, OLLAMA.1, 4)];
        let decision = decide_query_provider(EmbeddingProviderKind::Ollama, false, &stored);
        assert_eq!(
            decision,
            QueryProviderDecision::Block {
                active_provider: "bundled".to_string(),
                active_dim: 256,
                stored_provider: "ollama".to_string(),
                stored_dim: 768,
            }
        );
    }

    #[test]
    fn decide_ollama_unreachable_with_mixed_rows_blocks_on_foreign_space() {
        let stored = [space(BUNDLED.0, BUNDLED.1, 8), space(OLLAMA.0, OLLAMA.1, 2)];
        let decision = decide_query_provider(EmbeddingProviderKind::Ollama, false, &stored);
        assert_eq!(
            decision,
            QueryProviderDecision::Block {
                active_provider: "bundled".to_string(),
                active_dim: 256,
                stored_provider: "ollama".to_string(),
                stored_dim: 768,
            }
        );
    }

    #[test]
    fn vector_space_mismatch_display_carries_reembed_command() {
        let err = EmbeddingError::VectorSpaceMismatch {
            active_provider: "bundled".to_string(),
            active_dim: 256,
            stored_provider: "ollama".to_string(),
            stored_dim: 768,
        };
        let msg = format!("{err}");
        assert!(msg.contains("wonk update --force --provider bundled"));
    }

    // -- stored_vector_spaces / plan_query_provider ----------------------------

    use super::{fallback_after_disconnect, plan_query_provider, stored_vector_spaces};

    fn seed_space_rows(conn: &Connection, entries: &[(&str, usize)]) {
        for (i, (provider, dim)) in entries.iter().enumerate() {
            let sym_id = insert_symbol(
                conn,
                &format!("sym_{provider}_{i}"),
                "function",
                "a.rs",
                1,
                Some(1),
                None,
                "fn f()",
                "Rust",
            );
            conn.execute(
                "INSERT INTO embeddings
                    (symbol_id, file, chunk_text, vector, stale, created_at, provider, dim)
                 VALUES (?1, 'a.rs', 'chunk', x'00000000', 0, 1000, ?2, ?3)",
                rusqlite::params![sym_id, provider, *dim as i64],
            )
            .unwrap();
        }
    }

    #[test]
    fn stored_vector_spaces_empty_table_returns_empty() {
        let conn = setup_test_db_with_embeddings();
        assert!(stored_vector_spaces(&conn).unwrap().is_empty());
    }

    #[test]
    fn stored_vector_spaces_groups_and_orders_by_rows() {
        let conn = setup_test_db_with_embeddings();
        seed_space_rows(
            &conn,
            &[
                ("bundled", 256),
                ("bundled", 256),
                ("bundled", 256),
                ("ollama", 768),
            ],
        );

        let spaces = stored_vector_spaces(&conn).unwrap();
        assert_eq!(
            spaces,
            vec![
                StoredVectorSpace {
                    provider: "bundled".to_string(),
                    dim: 256,
                    rows: 3,
                },
                StoredVectorSpace {
                    provider: "ollama".to_string(),
                    dim: 768,
                    rows: 1,
                },
            ]
        );
    }

    #[test]
    fn plan_query_provider_bundled_index_stays_active_without_warning() {
        let conn = setup_test_db_with_embeddings();
        seed_space_rows(&conn, &[("bundled", 256), ("bundled", 256)]);

        let plan = plan_query_provider(&conn, EmbeddingProviderKind::Bundled).unwrap();
        assert_eq!(plan.provider.name(), "bundled");
        assert_eq!(plan.provider.dim(), 256);
        assert!(plan.fallback_warning.is_none());
    }

    #[test]
    fn plan_query_provider_bundled_config_over_foreign_index_blocks() {
        let conn = setup_test_db_with_embeddings();
        seed_space_rows(&conn, &[("ollama", 768)]);

        let err = match plan_query_provider(&conn, EmbeddingProviderKind::Bundled) {
            Err(e) => e,
            Ok(_) => panic!("expected VectorSpaceMismatch, got a provider plan"),
        };
        match err {
            EmbeddingError::VectorSpaceMismatch {
                active_provider,
                active_dim,
                stored_provider,
                stored_dim,
            } => {
                assert_eq!(active_provider, "bundled");
                assert_eq!(active_dim, 256);
                assert_eq!(stored_provider, "ollama");
                assert_eq!(stored_dim, 768);
            }
            other => panic!("expected VectorSpaceMismatch, got {other:?}"),
        }
    }

    // -- fallback_after_disconnect (mid-query re-plan) -------------------------

    #[test]
    fn fallback_after_disconnect_bundled_space_degrades_with_warning() {
        // The provider died mid-query over a bundled index: re-planning skips
        // the health check and degrades to the bundled provider.
        let conn = setup_test_db_with_embeddings();
        seed_space_rows(&conn, &[("bundled", 256), ("bundled", 256)]);

        let plan = fallback_after_disconnect(&conn, EmbeddingProviderKind::Ollama).unwrap();
        assert_eq!(plan.provider.name(), "bundled");
        assert_eq!(plan.provider.dim(), 256);
        assert_eq!(plan.fallback_warning, Some(BUNDLED_FALLBACK_WARNING));
    }

    #[test]
    fn fallback_after_disconnect_foreign_space_blocks() {
        // The stored vectors are foreign (ollama dim): degrading to bundled
        // would strand the indexed corpus, so the re-plan must refuse with
        // the re-embed instruction instead.
        let conn = setup_test_db_with_embeddings();
        seed_space_rows(&conn, &[("ollama", 768)]);

        let err = match fallback_after_disconnect(&conn, EmbeddingProviderKind::Ollama) {
            Err(e) => e,
            Ok(_) => panic!("expected VectorSpaceMismatch, got a provider plan"),
        };
        match err {
            EmbeddingError::VectorSpaceMismatch {
                active_provider,
                active_dim,
                stored_provider,
                stored_dim,
            } => {
                assert_eq!(active_provider, "bundled");
                assert_eq!(active_dim, 256);
                assert_eq!(stored_provider, "ollama");
                assert_eq!(stored_dim, 768);
            }
            other => panic!("expected VectorSpaceMismatch, got {other:?}"),
        }
    }
}
