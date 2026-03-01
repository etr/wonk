//! Application error types and user-facing error formatting.
//!
//! Provides structured error types for the query routing layer:
//! - [`DbError`] for database/index errors (enables fallback decisions)
//! - [`SearchError`] for grep-based search failures
//! - [`EmbeddingError`] for embedding / semantic-search failures
//! - [`WonkError`] as the unified top-level error type
//!
//! The [`WonkError`] type carries contextual hints and exit codes so that
//! `main()` can present human-readable diagnostics on stderr without ever
//! exposing raw panics or debug formatting.

use thiserror::Error;

// ---------------------------------------------------------------------------
// Exit codes
// ---------------------------------------------------------------------------

/// Process exit codes.
///
/// * `0` - success
/// * `1` - general runtime error
/// * `2` - usage / argument error (bad CLI invocation)
pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;

// ---------------------------------------------------------------------------
// Layer-specific error types
// ---------------------------------------------------------------------------

/// Errors arising from the SQLite index layer.
///
/// Pattern-matching on these variants drives the fallback logic in
/// [`crate::router::QueryRouter`]: when the index is missing or a query
/// returns no results, the router falls back to grep-based heuristics.
#[derive(Error, Debug)]
pub enum DbError {
    /// No index database exists for the current repository.
    #[error("no index found for this repository")]
    NoIndex,

    /// A SQL query failed at the rusqlite level.
    #[error("query failed: {0}")]
    QueryFailed(#[from] rusqlite::Error),
}

/// Errors arising from the grep-based text-search fallback.
#[derive(Error, Debug)]
pub enum SearchError {
    /// The grep search itself failed (e.g. bad pattern, I/O error).
    #[error("search failed: {0}")]
    SearchFailed(String),
}

/// Errors arising from the embedding / semantic-search layer.
#[derive(Error, Debug)]
pub enum EmbeddingError {
    /// Cannot connect to the Ollama server.
    #[error("cannot connect to Ollama embedding server")]
    OllamaUnreachable,

    /// Ollama returned an error response.
    #[error("Ollama error: {0}")]
    OllamaError(String),

    /// The response from Ollama could not be parsed.
    #[error("invalid embedding response")]
    InvalidResponse,

    /// No embeddings exist in the index.
    #[error("no embeddings found; run `wonk init --embed` to generate them")]
    NoEmbeddings,

    /// Failed to chunk a symbol body for embedding.
    #[error("failed to chunk symbol for embedding")]
    ChunkingFailed,

    /// A database operation in the embedding storage layer failed.
    #[error("embedding storage failed: {0}")]
    StorageFailed(String),
}

/// Errors arising from the LLM description generation layer.
#[derive(Error, Debug)]
pub enum LlmError {
    /// Cannot connect to the Ollama server.
    #[error("cannot connect to Ollama server")]
    OllamaUnreachable,

    /// The requested model is not available on the Ollama server.
    #[error("model not found: {0}")]
    ModelNotFound(String),

    /// Ollama returned an error response.
    #[error("Ollama error: {0}")]
    OllamaError(String),

    /// The response from Ollama could not be parsed.
    #[error("invalid LLM response")]
    InvalidResponse,
}

// ---------------------------------------------------------------------------
// Unified application error
// ---------------------------------------------------------------------------

/// Unified error type for the entire application.
///
/// Allows callers to propagate any layer's error through a single `Result`
/// type while still enabling pattern matching on the specific variant.
#[derive(Error, Debug)]
pub enum WonkError {
    #[error(transparent)]
    Db(#[from] DbError),

    #[error(transparent)]
    Search(#[from] SearchError),

    #[error(transparent)]
    Embedding(#[from] EmbeddingError),

    #[error(transparent)]
    Llm(#[from] LlmError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A usage / argument error (exit code 2).
    #[error("{0}")]
    Usage(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl WonkError {
    /// Return the appropriate process exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            WonkError::Usage(_) => EXIT_USAGE,
            _ => EXIT_ERROR,
        }
    }

    /// Return an optional human-readable hint that may help the user fix
    /// the problem.  Returns `None` when no specific guidance applies.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            WonkError::Db(DbError::NoIndex) => {
                Some("run `wonk init` to build an index for this repository")
            }
            WonkError::Db(DbError::QueryFailed(_)) => {
                Some("the index may be corrupt; try `wonk init` to rebuild it")
            }
            WonkError::Search(SearchError::SearchFailed(_)) => {
                Some("check your search pattern for syntax errors")
            }
            WonkError::Embedding(EmbeddingError::OllamaUnreachable) => {
                Some("ensure Ollama is running: `ollama serve`")
            }
            WonkError::Embedding(EmbeddingError::NoEmbeddings) => {
                Some("run `wonk init --embed` to generate embeddings")
            }
            WonkError::Embedding(EmbeddingError::StorageFailed(_)) => {
                Some("the index may be corrupt; try `wonk init` to rebuild it")
            }
            WonkError::Llm(LlmError::OllamaUnreachable) => {
                Some("ensure Ollama is running: `ollama serve`")
            }
            WonkError::Llm(LlmError::ModelNotFound(_)) => {
                Some("run `ollama pull <model>` or configure [llm].model in .wonk/config.toml")
            }
            WonkError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Some("verify the file or directory exists")
            }
            WonkError::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                Some("check file permissions")
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion: anyhow::Error -> WonkError  (for ? in dispatch)
// ---------------------------------------------------------------------------

// The `#[from] anyhow::Error` on `Other` already provides this via thiserror.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_usage() {
        let err = WonkError::Usage("bad flag".into());
        assert_eq!(err.exit_code(), EXIT_USAGE);
    }

    #[test]
    fn exit_code_general() {
        let err = WonkError::Db(DbError::NoIndex);
        assert_eq!(err.exit_code(), EXIT_ERROR);
    }

    #[test]
    fn exit_code_io() {
        let err = WonkError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert_eq!(err.exit_code(), EXIT_ERROR);
    }

    #[test]
    fn hint_no_index() {
        let err = WonkError::Db(DbError::NoIndex);
        assert!(err.hint().unwrap().contains("wonk init"));
    }

    #[test]
    fn hint_search_failed() {
        let err = WonkError::Search(SearchError::SearchFailed("oops".into()));
        assert!(err.hint().unwrap().contains("pattern"));
    }

    #[test]
    fn hint_io_not_found() {
        let err = WonkError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert!(err.hint().unwrap().contains("exists"));
    }

    #[test]
    fn hint_io_permission() {
        let err = WonkError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "nope",
        ));
        assert!(err.hint().unwrap().contains("permissions"));
    }

    #[test]
    fn hint_none_for_other() {
        let err = WonkError::Other(anyhow::anyhow!("something went wrong"));
        assert!(err.hint().is_none());
    }

    #[test]
    fn display_no_debug_formatting() {
        let err = WonkError::Db(DbError::NoIndex);
        let msg = format!("{err}");
        // Should be the human-readable message, not Debug output
        assert_eq!(msg, "no index found for this repository");
        assert!(!msg.contains("DbError"));
        assert!(!msg.contains("NoIndex"));
    }

    #[test]
    fn display_usage_error() {
        let err = WonkError::Usage("missing required argument: name".into());
        assert_eq!(format!("{err}"), "missing required argument: name");
    }

    #[test]
    fn db_error_display() {
        let err = DbError::NoIndex;
        assert_eq!(format!("{err}"), "no index found for this repository");
    }

    #[test]
    fn search_error_display() {
        let err = SearchError::SearchFailed("bad pattern".to_string());
        assert_eq!(format!("{err}"), "search failed: bad pattern");
    }

    #[test]
    fn wonk_error_from_db_error() {
        let db_err = DbError::NoIndex;
        let wonk_err: WonkError = db_err.into();
        assert!(matches!(wonk_err, WonkError::Db(DbError::NoIndex)));
    }

    #[test]
    fn wonk_error_from_search_error() {
        let search_err = SearchError::SearchFailed("oops".to_string());
        let wonk_err: WonkError = search_err.into();
        assert!(matches!(wonk_err, WonkError::Search(_)));
    }

    #[test]
    fn wonk_error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let wonk_err: WonkError = io_err.into();
        assert!(matches!(wonk_err, WonkError::Io(_)));
    }

    #[test]
    fn hint_query_failed() {
        let inner =
            rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(1), Some("test".into()));
        let err = WonkError::Db(DbError::QueryFailed(inner));
        assert!(err.hint().unwrap().contains("rebuild"));
    }

    // -- EmbeddingError tests -----------------------------------------------

    #[test]
    fn embedding_error_ollama_unreachable_display() {
        let err = EmbeddingError::OllamaUnreachable;
        assert_eq!(
            format!("{err}"),
            "cannot connect to Ollama embedding server"
        );
    }

    #[test]
    fn embedding_error_ollama_error_display() {
        let err = EmbeddingError::OllamaError("model not found".to_string());
        assert_eq!(format!("{err}"), "Ollama error: model not found");
    }

    #[test]
    fn embedding_error_invalid_response_display() {
        let err = EmbeddingError::InvalidResponse;
        assert_eq!(format!("{err}"), "invalid embedding response");
    }

    #[test]
    fn embedding_error_no_embeddings_display() {
        let err = EmbeddingError::NoEmbeddings;
        assert_eq!(
            format!("{err}"),
            "no embeddings found; run `wonk init --embed` to generate them"
        );
    }

    #[test]
    fn embedding_error_chunking_failed_display() {
        let err = EmbeddingError::ChunkingFailed;
        assert_eq!(format!("{err}"), "failed to chunk symbol for embedding");
    }

    #[test]
    fn wonk_error_from_embedding_error() {
        let emb_err = EmbeddingError::NoEmbeddings;
        let wonk_err: WonkError = emb_err.into();
        assert!(matches!(
            wonk_err,
            WonkError::Embedding(EmbeddingError::NoEmbeddings)
        ));
    }

    #[test]
    fn hint_ollama_unreachable() {
        let err = WonkError::Embedding(EmbeddingError::OllamaUnreachable);
        let hint = err.hint().unwrap();
        assert!(hint.contains("Ollama"));
    }

    #[test]
    fn hint_no_embeddings() {
        let err = WonkError::Embedding(EmbeddingError::NoEmbeddings);
        let hint = err.hint().unwrap();
        assert!(hint.contains("wonk init --embed"));
    }

    #[test]
    fn exit_code_embedding_error() {
        let err = WonkError::Embedding(EmbeddingError::NoEmbeddings);
        assert_eq!(err.exit_code(), EXIT_ERROR);
    }

    #[test]
    fn embedding_error_storage_failed_display() {
        let err = EmbeddingError::StorageFailed("disk full".to_string());
        assert_eq!(format!("{err}"), "embedding storage failed: disk full");
    }

    #[test]
    fn hint_storage_failed() {
        let err = WonkError::Embedding(EmbeddingError::StorageFailed("test".to_string()));
        let hint = err.hint().unwrap();
        assert!(hint.contains("rebuild"));
    }

    // -- LlmError tests -------------------------------------------------------

    #[test]
    fn llm_error_ollama_unreachable_display() {
        let err = LlmError::OllamaUnreachable;
        assert_eq!(format!("{err}"), "cannot connect to Ollama server");
    }

    #[test]
    fn llm_error_model_not_found_display() {
        let err = LlmError::ModelNotFound("llama3.2:3b".to_string());
        let msg = format!("{err}");
        assert!(msg.contains("llama3.2:3b"));
    }

    #[test]
    fn llm_error_ollama_error_display() {
        let err = LlmError::OllamaError("internal failure".to_string());
        assert_eq!(format!("{err}"), "Ollama error: internal failure");
    }

    #[test]
    fn llm_error_invalid_response_display() {
        let err = LlmError::InvalidResponse;
        assert_eq!(format!("{err}"), "invalid LLM response");
    }

    #[test]
    fn wonk_error_from_llm_error() {
        let llm_err = LlmError::OllamaUnreachable;
        let wonk_err: WonkError = llm_err.into();
        assert!(matches!(
            wonk_err,
            WonkError::Llm(LlmError::OllamaUnreachable)
        ));
    }

    #[test]
    fn hint_llm_ollama_unreachable() {
        let err = WonkError::Llm(LlmError::OllamaUnreachable);
        let hint = err.hint().unwrap();
        assert!(hint.contains("ollama serve"));
    }

    #[test]
    fn hint_llm_model_not_found() {
        let err = WonkError::Llm(LlmError::ModelNotFound("llama3.2:3b".to_string()));
        let hint = err.hint().unwrap();
        assert!(hint.contains("ollama pull"));
    }

    #[test]
    fn exit_code_llm_error() {
        let err = WonkError::Llm(LlmError::OllamaUnreachable);
        assert_eq!(err.exit_code(), EXIT_ERROR);
    }
}
